#!/bin/sh
# Copy the shared sources into each browser's package directory.
#
# Both browsers want a flat package with the manifest at its root, and neither
# follows a symlink out of one. Copying is what keeps a single source of truth
# for the files that would otherwise be maintained twice.
set -eu
cd "$(dirname "$0")"
for browser in chrome firefox; do
  rm -rf "$browser/shared"
  mkdir -p "$browser/shared"
  cp shared/*.js shared/*.html shared/*.css shared/*.png shared/*.woff2 "$browser/shared/"
done
