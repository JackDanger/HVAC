#!/usr/bin/env bats
# Tests for scripts/update-apt-repo.sh
#
# Run with: bats tests/apt_repo.bats
# Requires:  apt-ftparchive (apt-utils), ar, tar, gzip  — Ubuntu only.

SCRIPT="$BATS_TEST_DIRNAME/../scripts/update-apt-repo.sh"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# make_test_deb <name> <version> <arch> <output-dir>
# Creates a minimal but structurally valid .deb file that apt-ftparchive
# can read (correct ar members with a parseable control stanza).
make_test_deb() {
    local name="$1" version="$2" arch="$3" outdir="$4"
    local tmp; tmp=$(mktemp -d)

    printf '2.0\n' > "$tmp/debian-binary"

    # dpkg expects ./control (with leading ./) inside control.tar.gz.
    mkdir "$tmp/ctrl"
    printf 'Package: %s\nVersion: %s\nArchitecture: %s\nMaintainer: Test <t@t.invalid>\nDescription: Test package\n' \
        "$name" "$version" "$arch" > "$tmp/ctrl/control"
    (cd "$tmp/ctrl" && tar -czf "$tmp/control.tar.gz" ./control)

    # Empty data archive — apt-ftparchive only needs the control section.
    local empty; empty=$(mktemp -d)
    (cd "$empty" && tar -czf "$tmp/data.tar.gz" --files-from /dev/null 2>/dev/null || \
        tar -czf "$tmp/data.tar.gz" -T /dev/null 2>/dev/null || true)
    rmdir "$empty"

    ar r "$outdir/${name}_${version}_${arch}.deb" \
        "$tmp/debian-binary" \
        "$tmp/control.tar.gz" \
        "$tmp/data.tar.gz" 2>/dev/null

    rm -rf "$tmp"
}

# ---------------------------------------------------------------------------
# Setup / teardown
# ---------------------------------------------------------------------------

setup() {
    INCOMING=$(mktemp -d)
    REPO=$(mktemp -d)
    make_test_deb hvac 5.2.0 amd64 "$INCOMING"
    make_test_deb hvac 5.2.0 arm64 "$INCOMING"
}

teardown() {
    rm -rf "$INCOMING" "$REPO"
}

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

@test "creates pool directory containing the incoming .deb files" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    [ -f "$REPO/pool/main/hvac_5.2.0_amd64.deb" ]
    [ -f "$REPO/pool/main/hvac_5.2.0_arm64.deb" ]
}

@test "creates Packages index for amd64 containing the right package" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    [ -f "$REPO/dists/stable/main/binary-amd64/Packages" ]
    grep -q "Architecture: amd64" "$REPO/dists/stable/main/binary-amd64/Packages"
}

@test "creates Packages index for arm64 containing the right package" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    [ -f "$REPO/dists/stable/main/binary-arm64/Packages" ]
    grep -q "Architecture: arm64" "$REPO/dists/stable/main/binary-arm64/Packages"
}

@test "amd64 Packages does not contain arm64 entries" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    run grep "Architecture: arm64" "$REPO/dists/stable/main/binary-amd64/Packages"
    [ "$status" -ne 0 ]
}

@test "arm64 Packages does not contain amd64 entries" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    run grep "Architecture: amd64" "$REPO/dists/stable/main/binary-arm64/Packages"
    [ "$status" -ne 0 ]
}

@test "creates gzipped Packages files alongside plain ones" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    [ -f "$REPO/dists/stable/main/binary-amd64/Packages.gz" ]
    [ -f "$REPO/dists/stable/main/binary-arm64/Packages.gz" ]
}

@test "Packages Filename paths are relative to the repo root" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    grep -q "^Filename: pool/main/" "$REPO/dists/stable/main/binary-amd64/Packages"
}

@test "creates a Release file with correct Suite and Architectures fields" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    [ -f "$REPO/dists/stable/Release" ]
    grep -q "^Suite: stable" "$REPO/dists/stable/Release"
    grep -q "^Architectures: amd64 arm64" "$REPO/dists/stable/Release"
}

@test "Release file references both arch Packages files" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    grep -q "main/binary-amd64/Packages" "$REPO/dists/stable/Release"
    grep -q "main/binary-arm64/Packages" "$REPO/dists/stable/Release"
}

@test "accumulates packages across multiple releases" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"

    # Simulate a second release adding a new version.
    make_test_deb hvac 5.3.0 amd64 "$INCOMING"
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"

    pkg_count=$(grep -c "^Package:" "$REPO/dists/stable/main/binary-amd64/Packages")
    [ "$pkg_count" -ge 2 ]
}

@test "idempotent: re-running with the same input does not corrupt the repo" {
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    HVAC_APT_SKIP_SIGN=1 bash "$SCRIPT" "$INCOMING" "$REPO"
    grep -q "Architecture: amd64" "$REPO/dists/stable/main/binary-amd64/Packages"
    grep -q "Architecture: arm64" "$REPO/dists/stable/main/binary-arm64/Packages"
}
