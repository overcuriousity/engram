#!/bin/sh
# Copy the shared sources into each browser's package, as build.rs packs them.
set -eu
cd "$(dirname "$0")"
for browser in chrome firefox; do
  rm -rf "$browser/shared"
  mkdir -p "$browser/shared"
  cp shared/*.js shared/*.html shared/*.png "$browser/shared/"
  cat ../assets/css/00-tokens.css shared/panel.css > "$browser/shared/panel.css"
  cp ../assets/fonts/atkinson-next-400.woff2 ../assets/fonts/atkinson-next-500.woff2 "$browser/shared/"
done
