#!/usr/bin/env sh
# hvac installer — downloads the latest pre-built binary for your platform
# from https://github.com/JackDanger/hvac/releases and installs it, then
# installs ffmpeg + the right hardware-accel packages for the host (macOS
# via Homebrew, Debian/Ubuntu via apt).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/JackDanger/hvac/main/install.sh | sh
#
# Environment overrides:
#   HVAC_VERSION         pin to a specific tag (e.g. v5.1.1); default: latest
#   HVAC_PREFIX          install dir; default: /usr/local/bin if writable, else $HOME/.local/bin
#   HVAC_REPO            override owner/name; default: JackDanger/hvac
#   HVAC_SKIP_FFMPEG     set to 1 to skip ffmpeg / hardware-accel package install
#   HVAC_ASSUME_YES      set to 1 to skip the apt/brew confirmation prompt

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
# Try install(1) first, fall back to cp + chmod for hosts that lack it.
# Use an explicit if/elif chain rather than `install || cp && chmod`:
# that pattern parses as `(install || cp) && chmod`, and POSIX set -e is
# suppressed for non-final commands of an AND-OR list, so a `cp` failure
# would not exit the script.
if [ "${USE_SUDO:-0}" = "1" ]; then
    sudo install -m 755 "$tmp/hvac" "$PREFIX/hvac" \
        || err "failed to install hvac to $PREFIX (sudo install)"
elif install -m 755 "$tmp/hvac" "$PREFIX/hvac" 2>/dev/null; then
    :
elif cp "$tmp/hvac" "$PREFIX/hvac" 2>/dev/null && chmod 755 "$PREFIX/hvac"; then
    :
else
    err "failed to install hvac to $PREFIX (no write access, no install(1), and cp failed)"
fi

# ---- post-install hints ----------------------------------------------------

case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *) printf '\nNote: %s is not on your PATH. Add this to your shell rc:\n  export PATH="%s:$PATH"\n' \
              "$PREFIX" "$PREFIX" ;;
esac

# ---- ffmpeg + hardware acceleration ----------------------------------------
#
# hvac requires ffmpeg built with at least one GPU encoder: hevc_nvenc (NVIDIA),
# hevc_vaapi (Intel/AMD on Linux), or hevc_videotoolbox (macOS). The ffmpeg
# packages shipped by Homebrew and Debian/Ubuntu already enable all encoders
# the platform supports — we just need to install ffmpeg plus any userspace
# driver libraries the host GPU needs.

# Run a privileged command. Uses sudo only if we're not already root.
as_root() {
    if [ "$(id -u 2>/dev/null || echo 0)" -eq 0 ]; then
        "$@"
    elif command -v sudo >/dev/null 2>&1; then
        sudo "$@"
    else
        err "this step requires root; install sudo or run as root: $*"
    fi
}

# Detect distro id from /etc/os-release. Echoes one of: debian, ubuntu, other.
detect_linux_distro() {
    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        ID_LIKE=$(. /etc/os-release; printf '%s %s' "${ID:-}" "${ID_LIKE:-}")
        case " $ID_LIKE " in
            *" ubuntu "*) printf 'ubuntu\n'; return ;;
            *" debian "*) printf 'debian\n'; return ;;
        esac
    fi
    printf 'other\n'
}

# All apt packages installed? Used to short-circuit `apt-get update` +
# `install` when there's nothing to do — the common case for re-runs.
debs_all_installed() {
    for d in "$@"; do
        # dpkg-query is faster and quieter than `dpkg -s`. "ok installed"
        # is the canonical "fully installed" status.
        status=$(dpkg-query -W -f='${Status}' "$d" 2>/dev/null || true)
        case "$status" in
            "install ok installed") ;;
            *) return 1 ;;
        esac
    done
    return 0
}

confirm() {
    [ "${HVAC_ASSUME_YES:-0}" = "1" ] && return 0
    # No tty (piped from curl)? Default to yes so the one-liner install works.
    [ -t 0 ] || return 0
    printf '%s [Y/n] ' "$1"
    read -r reply
    case "$reply" in ''|y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

install_macos() {
    have_ffmpeg=0
    command -v ffmpeg >/dev/null 2>&1 && have_ffmpeg=1
    if [ "$have_ffmpeg" = "1" ]; then
        info "ffmpeg already installed; macOS hevc_videotoolbox is built into the OS"
        return 0
    fi
    if ! command -v brew >/dev/null 2>&1; then
        info "Homebrew not found; install it from https://brew.sh and re-run, or:"
        info "  brew install ffmpeg"
        return 0
    fi
    if confirm "Install ffmpeg via 'brew install ffmpeg'?"; then
        info "running: brew install ffmpeg"
        brew install ffmpeg
    else
        info "skipped ffmpeg install; run 'brew install ffmpeg' when ready"
    fi
}

install_debian_ubuntu() {
    distro="$1"

    # Install ffmpeg + the VAAPI driver stack unconditionally. The drivers
    # are ~5 MB combined, harmless on hosts without a matching GPU, and
    # dropping the lspci-based gating means this also works in minimal
    # containers where pciutils isn't installed. NVIDIA's kernel driver is
    # *not* auto-installed — reboots + Optimus/Tesla/container-host quirks
    # make that too invasive to do silently; we hint instead if the device
    # is present without a driver.
    #
    # intel-media-va-driver: Broadwell+ Intel iGPUs.
    # mesa-va-drivers:       AMD + older Intel (i965, ironlake).
    # vainfo:                diagnostics + hvac's startup VAAPI probe.
    pkgs="ffmpeg vainfo intel-media-va-driver mesa-va-drivers"

    if debs_all_installed $pkgs; then
        info "ffmpeg + VAAPI drivers already installed (skipping apt)"
    else
        info "will install via apt: $pkgs"
        if ! confirm "Proceed with 'apt-get install' for the packages above?"; then
            info "skipped ffmpeg install; re-run with HVAC_ASSUME_YES=1 to auto-confirm"
            return 0
        fi
        as_root env DEBIAN_FRONTEND=noninteractive apt-get update -qq
        # shellcheck disable=SC2086
        as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $pkgs
    fi

    # Use /dev/nvidia0 (not lspci) so this works in containers that pass the
    # device through without pciutils. If the device is exposed but the
    # userland driver isn't visible, point the user at the right package.
    if [ -e /dev/nvidia0 ] && ! command -v nvidia-smi >/dev/null 2>&1; then
        cat <<EOF

Note: /dev/nvidia0 exists but nvidia-smi is not on PATH, so the proprietary
driver does not look installed. hvac needs it for hevc_nvenc:
  $distro: sudo apt install nvidia-driver       # then reboot
Cloud / container hosts: install the matching driver from your provider.
EOF
    fi
}

install_other_linux_hint() {
    cat <<'EOF'

ffmpeg / hardware-accel packages were not auto-installed (unsupported
distro for this script). Install ffmpeg manually:
  Arch:    sudo pacman -S ffmpeg libva-utils
  Fedora:  sudo dnf install ffmpeg-free libva-utils
  Other:   see your distro's packaging
EOF
}

if [ "${HVAC_SKIP_FFMPEG:-0}" != "1" ]; then
    info "checking ffmpeg + hardware acceleration"
    case "$OS" in
        Darwin)
            install_macos
            ;;
        Linux)
            distro=$(detect_linux_distro)
            case "$distro" in
                ubuntu|debian) install_debian_ubuntu "$distro" ;;
                *)             install_other_linux_hint ;;
            esac
            ;;
    esac
fi

# ---- final summary ---------------------------------------------------------
#
# A bare "Installed hvac" doesn't tell the user whether the install is
# actually usable. Probe ffmpeg's encoder list and report which HEVC
# hardware encoders the just-installed ffmpeg can hand to hvac, so a
# misconfigured host (e.g. ffmpeg-free without nvenc) is caught here
# instead of on first run. We never *fail* on a missing encoder — the
# user may be installing hvac on a build host or a NAS scheduler that
# proxies work elsewhere — but we make it loud.

probe_hevc_encoders() {
    command -v ffmpeg >/dev/null 2>&1 || { printf ''; return; }
    # `ffmpeg -encoders` prints one encoder per line; filter to the three
    # HEVC hardware encoders hvac drives (nvenc/vaapi/videotoolbox — see
    # the "GPU required" table in the README).
    ffmpeg -hide_banner -encoders 2>/dev/null \
        | awk '/ hevc_(nvenc|vaapi|videotoolbox) / {print $2}' \
        | paste -sd ',' -
}

hvac_version=$("$PREFIX/hvac" --version 2>/dev/null || printf 'hvac')
ffmpeg_version=$(ffmpeg -version 2>/dev/null | awk 'NR==1 {print $1" "$3}')
encoders=$(probe_hevc_encoders)

printf '\n'
info "$hvac_version installed at $PREFIX/hvac"
if [ -n "$ffmpeg_version" ]; then
    if [ -n "$encoders" ]; then
        info "$ffmpeg_version — HEVC encoders available: $encoders"
    else
        printf 'warning: %s found, but none of hevc_nvenc/hevc_vaapi/hevc_videotoolbox\n' "$ffmpeg_version" >&2
        printf '         are compiled in. hvac will refuse to start. Reinstall ffmpeg\n' >&2
        printf '         from a build that enables your platform encoder.\n' >&2
    fi
else
    printf 'warning: ffmpeg not on PATH — hvac requires it at runtime.\n' >&2
fi
printf '\nTry:\n  hvac --help\n'
