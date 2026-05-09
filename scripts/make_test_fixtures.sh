#!/bin/bash
# Creates test ISO fixtures representing the diversity of disc image structures.
# Run on a machine with ffmpeg and genisoimage installed.
# Output: tests/fixtures/*.iso
set -e

OUTDIR="${1:-/tmp/hvecuum_fixtures}"
mkdir -p "$OUTDIR"

# Helper: create a small H.264 mpegts file
make_video() {
  local path="$1" duration="${2:-10}" size="${3:-720x480}" bitrate="${4:-500k}"
  ffmpeg -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc2=duration=${duration}:size=${size}:rate=24" \
    -f lavfi -i "sine=frequency=440:duration=${duration}" \
    -c:v libx264 -preset ultrafast -b:v "$bitrate" \
    -c:a aac -b:a 64k \
    -f mpegts \
    "$path"
}

# Helper: create a small VOB-like file (MPEG-2 PS for DVD)
make_vob() {
  local path="$1" duration="${2:-10}" size="${3:-720x480}" bitrate="${4:-500k}"
  ffmpeg -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc2=duration=${duration}:size=${size}:rate=24" \
    -f lavfi -i "sine=frequency=440:duration=${duration}" \
    -c:v mpeg2video -b:v "$bitrate" \
    -c:a mp2 -b:a 128k \
    -f vob \
    "$path"
}

###############################################################################
# 1. BDMV_DISC.iso — Standard Blu-ray with multi-chapter main feature + extras
###############################################################################
echo "=== Building BDMV_DISC.iso ==="
T=$(mktemp -d)
mkdir -p "$T/BDMV/STREAM" "$T/BDMV/PLAYLIST" "$T/BDMV/CLIPINF" "$T/BDMV/AUXDATA" "$T/BDMV/BACKUP" "$T/CERTIFICATE"

# Main feature split across 3 chapter files (largest files)
make_video "$T/BDMV/STREAM/00000.m2ts" 60 1920x1080 1500k   # ch1 - biggest
make_video "$T/BDMV/STREAM/00001.m2ts" 45 1920x1080 1500k   # ch2
make_video "$T/BDMV/STREAM/00002.m2ts" 50 1920x1080 1500k   # ch3

# Bonus features (smaller)
make_video "$T/BDMV/STREAM/00010.m2ts" 15 1920x1080 800k    # behind the scenes
make_video "$T/BDMV/STREAM/00011.m2ts" 8  1920x1080 800k    # trailer

# Tiny menu/navigation clips
make_video "$T/BDMV/STREAM/00100.m2ts" 2  1920x1080 200k    # menu loop

# Create fake playlist and index files (content doesn't matter, structure does)
echo "MPLS" > "$T/BDMV/PLAYLIST/00000.mpls"
echo "MPLS" > "$T/BDMV/PLAYLIST/00001.mpls"
echo "BDMV0200" > "$T/BDMV/index.bdmv"
echo "MOBJ0200" > "$T/BDMV/MovieObject.bdmv"

genisoimage -udf -V "BDMV_DISC" -o "$OUTDIR/BDMV_DISC.iso" "$T"
ls -lh "$OUTDIR/BDMV_DISC.iso"
rm -rf "$T"

###############################################################################
# 2. DVD_DISC.iso — Standard DVD with VIDEO_TS structure
###############################################################################
echo "=== Building DVD_DISC.iso ==="
T=$(mktemp -d)
mkdir -p "$T/VIDEO_TS"

# VTS_01 = main feature (3 VOBs, split like a real DVD)
make_vob "$T/VIDEO_TS/VTS_01_1.VOB" 40 720x480 600k
make_vob "$T/VIDEO_TS/VTS_01_2.VOB" 40 720x480 600k
make_vob "$T/VIDEO_TS/VTS_01_3.VOB" 30 720x480 600k

# VTS_02 = bonus feature (1 smaller VOB)
make_vob "$T/VIDEO_TS/VTS_02_1.VOB" 15 720x480 400k

# Menu VOB
make_vob "$T/VIDEO_TS/VIDEO_TS.VOB" 3  720x480 200k

# IFO/BUP stubs (real DVDs have binary data here, but structure is what matters)
echo "DVDVIDEO-VMG" > "$T/VIDEO_TS/VIDEO_TS.IFO"
echo "DVDVIDEO-VMG" > "$T/VIDEO_TS/VIDEO_TS.BUP"
echo "DVDVIDEO-VTS" > "$T/VIDEO_TS/VTS_01_0.IFO"
echo "DVDVIDEO-VTS" > "$T/VIDEO_TS/VTS_01_0.BUP"
echo "DVDVIDEO-VTS" > "$T/VIDEO_TS/VTS_02_0.IFO"
echo "DVDVIDEO-VTS" > "$T/VIDEO_TS/VTS_02_0.BUP"

genisoimage -udf -V "DVD_DISC" -o "$OUTDIR/DVD_DISC.iso" "$T"
ls -lh "$OUTDIR/DVD_DISC.iso"
rm -rf "$T"

###############################################################################
# 3. AVCHD_DISC.iso — AVCHD camcorder structure (PRIVATE/AVCHD/BDMV/STREAM)
###############################################################################
echo "=== Building AVCHD_DISC.iso ==="
T=$(mktemp -d)
mkdir -p "$T/PRIVATE/AVCHD/BDMV/STREAM" "$T/PRIVATE/AVCHD/BDMV/CLIPINF" "$T/PRIVATE/AVCHD/BDMV/PLAYLIST"

# Camcorder clips — sequential recording
make_video "$T/PRIVATE/AVCHD/BDMV/STREAM/00000.MTS" 30 1920x1080 1000k
make_video "$T/PRIVATE/AVCHD/BDMV/STREAM/00001.MTS" 25 1920x1080 1000k
make_video "$T/PRIVATE/AVCHD/BDMV/STREAM/00002.MTS" 20 1920x1080 1000k

echo "MPLS" > "$T/PRIVATE/AVCHD/BDMV/PLAYLIST/00000.mpls"

genisoimage -udf -V "AVCHD_DISC" -o "$OUTDIR/AVCHD_DISC.iso" "$T"
ls -lh "$OUTDIR/AVCHD_DISC.iso"
rm -rf "$T"

###############################################################################
# 4. BARE_MEDIA.iso — No standard structure, just media files at root/subdir
###############################################################################
echo "=== Building BARE_MEDIA.iso ==="
T=$(mktemp -d)
mkdir -p "$T/videos"

make_video "$T/movie.mkv" 60 1280x720 800k
make_video "$T/videos/bonus.mkv" 20 1280x720 500k
make_video "$T/trailer.mp4" 5 1280x720 300k

genisoimage -udf -V "BARE_MEDIA" -o "$OUTDIR/BARE_MEDIA.iso" "$T"
ls -lh "$OUTDIR/BARE_MEDIA.iso"
rm -rf "$T"

###############################################################################
# 5. MULTI_TITLE_DVD.iso — DVD with multiple equally-sized title sets
#    (e.g. TV episodes disc)
###############################################################################
echo "=== Building MULTI_TITLE_DVD.iso ==="
T=$(mktemp -d)
mkdir -p "$T/VIDEO_TS"

# 4 title sets of similar size (TV episodes)
for i in 01 02 03 04; do
  make_vob "$T/VIDEO_TS/VTS_${i}_1.VOB" 25 720x480 500k
done

# Menu
make_vob "$T/VIDEO_TS/VIDEO_TS.VOB" 3 720x480 200k
echo "DVDVIDEO-VMG" > "$T/VIDEO_TS/VIDEO_TS.IFO"
for i in 01 02 03 04; do
  echo "DVDVIDEO-VTS" > "$T/VIDEO_TS/VTS_${i}_0.IFO"
done

genisoimage -udf -V "MULTI_TITLE_DVD" -o "$OUTDIR/MULTI_TITLE_DVD.iso" "$T"
ls -lh "$OUTDIR/MULTI_TITLE_DVD.iso"
rm -rf "$T"

###############################################################################
# 6. BDMV_FLAT.iso — Blu-ray where BDMV is the ISO root (no parent dir)
###############################################################################
echo "=== Building BDMV_FLAT.iso ==="
T=$(mktemp -d)
mkdir -p "$T/STREAM" "$T/PLAYLIST" "$T/CLIPINF"

# Two chapter files
make_video "$T/STREAM/00000.m2ts" 45 1920x1080 1200k
make_video "$T/STREAM/00001.m2ts" 40 1920x1080 1200k

# Small extra
make_video "$T/STREAM/00010.m2ts" 10 1920x1080 600k

echo "BDMV0200" > "$T/index.bdmv"

genisoimage -udf -V "BDMV_FLAT" -o "$OUTDIR/BDMV_FLAT.iso" "$T"
ls -lh "$OUTDIR/BDMV_FLAT.iso"
rm -rf "$T"

echo ""
echo "=== All fixtures built in $OUTDIR ==="
ls -lh "$OUTDIR"/*.iso
echo ""
echo "To install: cp $OUTDIR/*.iso tests/fixtures/"
