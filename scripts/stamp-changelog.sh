#!/bin/sh
# Turn `## [Unreleased]` into `## [<version>] - <date>` in CHANGELOG.md and print that section.
set -eu
version=${1:?usage: stamp-changelog.sh <version> [changelog]}
file=${2:-CHANGELOG.md}
grep -q '^## \[Unreleased\]' "$file" || { echo "no '## [Unreleased]' heading in $file" >&2; exit 1; }
date=$(date -u +%Y-%m-%d)
tmp=$(mktemp)
sed "0,/^## \[Unreleased\]/s//## [$version] - $date/" "$file" > "$tmp"
mv "$tmp" "$file"
section=$(awk -v v="## [$version]" 'index($0, v) == 1 { on = 1; next } /^## \[/ && on { exit } on' "$file")
if [ -z "$(printf '%s' "$section" | tr -d '[:space:]')" ]; then
  echo "No user-visible changes."
else
  printf '%s\n' "$section"
fi
