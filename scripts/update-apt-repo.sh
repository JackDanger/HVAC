#!/usr/bin/env bash
# update-apt-repo.sh <incoming-debs-dir> <repo-root-dir>
#
# Copies new .deb files into pool/main, regenerates the per-architecture
# Packages indices, and signs Release/InRelease with the GPG key already
# imported in the running keyring.
#
# Environment:
#   HVAC_APT_SKIP_SIGN=1   skip GPG signing (used in tests)
#
# Requires: apt-ftparchive (apt-utils), gpg
set -euo pipefail

INCOMING="${1:?usage: $0 <incoming-debs-dir> <repo-root-dir>}"
REPO="${2:?usage: $0 <incoming-debs-dir> <repo-root-dir>}"

SUITE="stable"
COMPONENT="main"

# Copy incoming .deb files into the pool.
mkdir -p "$REPO/pool/$COMPONENT"
find "$INCOMING" -maxdepth 1 -name '*.deb' | while read -r deb; do
    cp "$deb" "$REPO/pool/$COMPONENT/"
done

# Build a combined packages listing from the pool, then split it by
# architecture.  apt-ftparchive packages outputs one blank-line-separated
# RFC 2822 stanza per .deb; awk paragraph mode (RS="") lets us match each
# stanza against its Architecture: field without reading the file twice.
all_pkgs=$(mktemp)
(cd "$REPO" && apt-ftparchive packages "pool/$COMPONENT") > "$all_pkgs"

for arch in amd64 arm64; do
    pkg_dir="$REPO/dists/$SUITE/$COMPONENT/binary-$arch"
    mkdir -p "$pkg_dir"
    awk -v arch="$arch" 'BEGIN{RS=""; ORS="\n\n"} $0 ~ "Architecture: " arch' \
        "$all_pkgs" > "$pkg_dir/Packages"
    gzip -9 -k -f "$pkg_dir/Packages"
done
rm -f "$all_pkgs"

# Generate the Release file, which lists all index files with their checksums.
apt-ftparchive \
    -o "APT::FTPArchive::Release::Origin=hvac" \
    -o "APT::FTPArchive::Release::Label=hvac" \
    -o "APT::FTPArchive::Release::Suite=$SUITE" \
    -o "APT::FTPArchive::Release::Codename=$SUITE" \
    -o "APT::FTPArchive::Release::Architectures=amd64 arm64" \
    -o "APT::FTPArchive::Release::Components=$COMPONENT" \
    -o "APT::FTPArchive::Release::Description=hvac GPU-accelerated HEVC transcoder" \
    release "$REPO/dists/$SUITE" \
    > "$REPO/dists/$SUITE/Release"

[ "${HVAC_APT_SKIP_SIGN:-0}" = "1" ] && exit 0

# Sign the Release file.  The caller is responsible for importing the key.
KEY_ID=$(gpg --list-secret-keys --with-colons | awk -F: '/^sec:/ { print $5; exit }')
gpg --batch --yes --armor \
    --local-user "$KEY_ID" \
    --detach-sign --output "$REPO/dists/$SUITE/Release.gpg" \
    "$REPO/dists/$SUITE/Release"
gpg --batch --yes --armor \
    --local-user "$KEY_ID" \
    --clearsign --output "$REPO/dists/$SUITE/InRelease" \
    "$REPO/dists/$SUITE/Release"
