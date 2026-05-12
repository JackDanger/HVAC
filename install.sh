#!/usr/bin/env sh
# hvac installer — on Debian/Ubuntu uses the apt repository; on macOS uses
# Homebrew; everywhere else downloads a pre-built binary tarball from
# https://github.com/JackDanger/hvac/releases and installs ffmpeg with the
# right hardware-accel packages for the host.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/JackDanger/hvac/main/install.sh | sh
#
# Environment overrides:
#   HVAC_VERSION         pin to a specific tag (e.g. v5.1.1); default: latest
#   HVAC_PREFIX          install dir for tarball path; default: /usr/local/bin
#   HVAC_REPO            override GitHub owner/name; default: JackDanger/hvac
#   HVAC_SKIP_APT        set to 1 to skip the apt repo path on Debian/Ubuntu
#   HVAC_SKIP_BREW       set to 1 to skip the Homebrew path on macOS
#   HVAC_SKIP_FFMPEG     set to 1 to skip ffmpeg / hardware-accel install
#   HVAC_ASSUME_YES      set to 1 to skip all confirmation prompts

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

# Prefer native package managers over the tarball where available:
#   Debian/Ubuntu → apt (our gh-pages repo)
#   macOS + Homebrew → brew tap
# Both paths skip the tarball download and get upgrades via the package manager.
_use_apt=0
_use_brew=0
if [ "$OS" = "Linux" ] && [ "${HVAC_SKIP_APT:-0}" != "1" ] && [ -r /etc/os-release ]; then
    _apt_id=$(. /etc/os-release 2>/dev/null && printf '%s %s' "${ID:-}" "${ID_LIKE:-}")
    case " $_apt_id " in
        *" ubuntu "*|*" debian "*|*" openmediavault "*) _use_apt=1 ;;
    esac
fi
if [ "$OS" = "Darwin" ] && [ "${HVAC_SKIP_BREW:-0}" != "1" ] && command -v brew >/dev/null 2>&1; then
    _use_brew=1
fi

if [ "$_use_apt" = "0" ] && [ "$_use_brew" = "0" ]; then

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

fi # _use_apt=0 / _use_brew=0

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

# Detect distro id from /etc/os-release plus a couple of NAS-specific
# files. Echoes one of: debian, ubuntu, alpine, synology, qnap, unraid,
# openmediavault, other.
#
# NAS detection runs before /etc/os-release because Synology's os-release
# claims ID=dsm (sometimes ID_LIKE=debian, sometimes nothing) and QNAP's
# QTS has no os-release at all. Catching them by their canonical
# fingerprint files keeps the apt branch from running on a host where
# apt either doesn't exist or would break the appliance.
detect_linux_distro() {
    # Synology DSM: /etc.defaults/VERSION is present on every model.
    if [ -r /etc.defaults/VERSION ] || [ -r /etc/synoinfo.conf ]; then
        printf 'synology\n'; return
    fi
    # QNAP QTS: /etc/config/uLinux.conf is the well-known fingerprint.
    if [ -r /etc/config/uLinux.conf ]; then
        printf 'qnap\n'; return
    fi
    # Unraid: rootfs is read-only Slackware loaded into RAM; /etc/unraid-version exists.
    if [ -r /etc/unraid-version ]; then
        printf 'unraid\n'; return
    fi
    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        ID_LIKE=$(. /etc/os-release; printf '%s %s' "${ID:-}" "${ID_LIKE:-}")
        case " $ID_LIKE " in
            # OpenMediaVault declares ID=openmediavault, ID_LIKE=debian.
            # Surface it as 'openmediavault' (matches the dispatch case
            # below) so we can point users at omv-extras / the OMV docker
            # plugin instead of plain apt — but the apt path itself still
            # works, so this is informational.
            *" openmediavault "*) printf 'openmediavault\n'; return ;;
            *" ubuntu "*) printf 'ubuntu\n'; return ;;
            *" debian "*) printf 'debian\n'; return ;;
            *" alpine "*) printf 'alpine\n'; return ;;
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
    # Called from the brew-install block (below) when _use_brew=1, and from
    # the HVAC_SKIP_FFMPEG block when _use_brew=0 (tarball path, ffmpeg hint).
    if [ "$_use_brew" = "1" ]; then
        if brew ls --versions hvac >/dev/null 2>&1; then
            info "hvac already installed via Homebrew (run 'brew upgrade hvac' to update)"
        else
            info "will install via Homebrew: JackDanger/tap/hvac (ffmpeg included as dependency)"
            if ! confirm "Proceed with 'brew install JackDanger/tap/hvac'?"; then
                info "skipped; run 'brew install JackDanger/tap/hvac' when ready"
                return 0
            fi
            brew install JackDanger/tap/hvac
        fi
        return 0
    fi
    # No Homebrew — binary was installed via tarball above; only ffmpeg is missing.
    if command -v ffmpeg >/dev/null 2>&1; then
        info "ffmpeg already installed; macOS hevc_videotoolbox is built into the OS"
        return 0
    fi
    info "Homebrew not found. Install it from https://brew.sh, then run:"
    info "  brew install JackDanger/tap/hvac"
    info "Or install ffmpeg manually: https://ffmpeg.org/download.html"
}

install_debian_ubuntu() {
    distro="$1"
    _hvac_keyring="/etc/apt/keyrings/hvac.gpg"
    _hvac_source="/etc/apt/sources.list.d/hvac.list"
    _hvac_apt_url="https://jackdanger.github.io/HVAC"

    if [ ! -f "$_hvac_keyring" ]; then
        info "adding hvac apt signing key"
        as_root mkdir -p /etc/apt/keyrings
        download "$_hvac_apt_url/key.gpg" - \
            | as_root gpg --batch --yes --dearmor -o "$_hvac_keyring"
    fi

    if [ ! -f "$_hvac_source" ]; then
        info "adding hvac apt source"
        printf 'deb [signed-by=%s] %s stable main\n' \
            "$_hvac_keyring" "$_hvac_apt_url" \
            | as_root tee "$_hvac_source" > /dev/null
    fi

    if debs_all_installed hvac; then
        info "hvac already installed (run 'sudo apt upgrade hvac' to update)"
    else
        info "will install via apt: hvac (ffmpeg + VAAPI drivers pulled in via dependencies)"
        if ! confirm "Proceed with 'apt-get install hvac'?"; then
            info "skipped; re-run with HVAC_ASSUME_YES=1 to auto-confirm"
            return 0
        fi
        as_root env DEBIAN_FRONTEND=noninteractive apt-get update -qq
        as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y hvac
    fi

    # Use /dev/nvidia0 (not lspci) so this works in containers that pass the
    # device through without pciutils.
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
  Alpine:  sudo apk add ffmpeg libva-utils
  Other:   see your distro's packaging
EOF
}

# Synology DSM: there's no apt, ffmpeg from Package Center is gimped (no
# encoders), and most models have no GPU at all. The actionable path is
# Docker on Plus models (Intel iGPU passes through to /dev/dri), or
# transcoding off-box.
install_synology_hint() {
    cat <<'EOF'

Detected Synology DSM. This installer cannot configure ffmpeg or GPU
drivers on Synology — the Package Center ffmpeg lacks the HEVC encoders
hvac needs. Two supported paths:

  1. Docker (recommended). Install "Container Manager" from Package
     Center, then follow docs/NAS.md#synology for the docker run / compose
     snippet. Plus models pass the Intel iGPU through at /dev/dri.

  2. Off-box. Run hvac on a Linux box / Mac with a GPU and point it at the
     Synology share via NFS or SMB. Expect transcode throughput to be
     bound by the network, not the GPU — 1 GbE is enough for one job.

Either way the binary at $PREFIX/hvac is harmless to leave installed;
it just won't have a usable encoder on DSM itself.
EOF
}

# QNAP QTS: similar story to Synology — Entware ships an ffmpeg, but the
# Plex / hardware-transcoding story on QNAP is so model-dependent that
# pointing at Container Station is the only general advice.
install_qnap_hint() {
    cat <<'EOF'

Detected QNAP QTS. This installer cannot configure ffmpeg or GPU drivers
on QTS. The supported path is Container Station:

  1. Install "Container Station" from the App Center.
  2. Follow docs/NAS.md#qnap for the docker run / compose snippet.
     Intel-based TS-x53 / TS-x73 / TS-h-series pass /dev/dri through;
     ARM models have no usable HEVC encoder.

Off-box transcoding (NFS / SMB mount on a Linux host) also works.
EOF
}

# Unraid: rootfs is RAM-loaded so anything we install vanishes on reboot.
# Community Applications + the Docker tab is the canonical pattern.
install_unraid_hint() {
    cat <<'EOF'

Detected Unraid. The rootfs is loaded from /boot at every reboot, so
installing ffmpeg into / will not survive. The supported path is Docker:

  1. From the WebUI's Apps tab, install Community Applications if not
     already present.
  2. Follow docs/NAS.md#unraid for the container template.
     Intel iGPUs need the "Intel-GPU-TOP" plugin to expose /dev/dri.
     NVIDIA GPUs need the "Nvidia-Driver" plugin from CA.

The binary at $PREFIX/hvac will be gone after the next reboot. Drop the
docker container in instead.
EOF
}

# OpenMediaVault sits on top of Debian, so apt works — the hint just
# points at omv-extras + docker-compose, the idiomatic OMV path.
install_omv_hint() {
    cat <<'EOF'

Detected OpenMediaVault (Debian under the hood). The standard apt install
will work, but the OMV-idiomatic path is the "compose" plugin from
omv-extras — see docs/NAS.md#openmediavault.
EOF
}

install_alpine() {
    pkgs="ffmpeg libva-utils mesa-va-gallium intel-media-driver"
    info "will install via apk: $pkgs"
    if ! confirm "Proceed with 'apk add' for the packages above?"; then
        info "skipped ffmpeg install; re-run with HVAC_ASSUME_YES=1 to auto-confirm"
        return 0
    fi
    # shellcheck disable=SC2086
    as_root apk add --no-cache $pkgs
}

# ---- install via native package manager (brew / apt) ----------------------
#
# These paths install hvac itself and are not gated on HVAC_SKIP_FFMPEG
# because the package manager handles ffmpeg as a dependency.
if [ "$_use_brew" = "1" ]; then
    install_macos
elif [ "$_use_apt" = "1" ]; then
    distro=$(detect_linux_distro)
    case "$distro" in
        openmediavault) install_omv_hint; install_debian_ubuntu debian ;;
        *)              install_debian_ubuntu "$distro" ;;
    esac
fi

# ---- ffmpeg + hardware acceleration (tarball-path platforms) ---------------
#
# For platforms that received hvac via tarball above (non-Debian Linux, macOS
# without Homebrew), install ffmpeg and any needed GPU driver packages.
if [ "${HVAC_SKIP_FFMPEG:-0}" != "1" ] && [ "$_use_brew" = "0" ] && [ "$_use_apt" = "0" ]; then
    info "checking ffmpeg + hardware acceleration"
    case "$OS" in
        Darwin)
            install_macos
            ;;
        Linux)
            distro=$(detect_linux_distro)
            case "$distro" in
                alpine)           install_alpine ;;
                synology)         install_synology_hint ;;
                qnap)             install_qnap_hint ;;
                unraid)           install_unraid_hint ;;
                *)                install_other_linux_hint ;;
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

_hvac_bin=$(command -v hvac 2>/dev/null || printf '%s/hvac' "${PREFIX:-/usr/local/bin}")
hvac_version=$("$_hvac_bin" --version 2>/dev/null || printf 'hvac')
ffmpeg_version=$(ffmpeg -version 2>/dev/null | awk 'NR==1 {print $1" "$3}')
encoders=$(probe_hevc_encoders)

printf '\n'
info "$hvac_version installed at $_hvac_bin"
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
