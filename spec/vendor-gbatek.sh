#!/bin/bash
# Download and verify GBATEK. Run once from the repo root; the result
# (spec/gbatek.htm) is committed to the repo so builds are reproducible
# and the container's build step is offline-deterministic.
#
# Usage: ./spec/vendor-gbatek.sh
set -euo pipefail

URL="https://problemkaputt.de/gbatek.htm"
OUT="spec/gbatek.htm"
EXPECTED_SHA256="919ca0deac2fe11ecc4eda95fd95157772dd72e2ad69fff61bdbaa9fb53121c8"

curl -fsSL "$URL" -o "$OUT.tmp"

ACTUAL=$(shasum -a 256 "$OUT.tmp" | awk '{print $1}')
if [ "$ACTUAL" != "$EXPECTED_SHA256" ]; then
    echo "SHA256 mismatch!" >&2
    echo "  expected: $EXPECTED_SHA256" >&2
    echo "  actual:   $ACTUAL" >&2
    echo "If upstream changed intentionally, update EXPECTED_SHA256 after review." >&2
    rm "$OUT.tmp"
    exit 1
fi

mv "$OUT.tmp" "$OUT"
echo "Vendored $OUT ($(wc -c < "$OUT") bytes, sha256=$ACTUAL)"
