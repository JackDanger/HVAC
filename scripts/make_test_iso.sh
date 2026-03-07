#!/bin/bash
set -e
TMPDIR=$(mktemp -d)
mkdir -p "$TMPDIR/BDMV/STREAM"

# Create a ~12MB H.264 test video (1080p, 60s, higher bitrate)
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=duration=60:size=1920x1080:rate=30" \
  -f lavfi -i "sine=frequency=440:duration=60" \
  -c:v libx264 -preset ultrafast -b:v 1500k \
  -c:a aac -b:a 128k \
  -f mpegts \
  "$TMPDIR/BDMV/STREAM/00000.m2ts"

ls -lh "$TMPDIR/BDMV/STREAM/00000.m2ts"

# Create ISO with UDF filesystem (Blu-ray uses UDF)
genisoimage -udf -V "BDMV_DISC" \
  -o "$TMPDIR/BDMV_DISC.iso" \
  "$TMPDIR/BDMV"

ls -lh "$TMPDIR/BDMV_DISC.iso"

# Move to a known location
cp "$TMPDIR/BDMV_DISC.iso" /tmp/BDMV_DISC.iso
rm -rf "$TMPDIR"
echo "DONE: /tmp/BDMV_DISC.iso"
