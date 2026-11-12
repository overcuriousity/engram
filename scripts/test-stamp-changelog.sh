#!/bin/sh
# Checks stamp-changelog.sh: the heading moves, the notes come out, a missing heading fails.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

printf '# Changelog\n\n## [Unreleased]\n\n### Changed\n- A thing.\n\n## [2026.1101.0] - 2026-11-01\n- Older.\n' > "$dir/c.md"
notes=$("$here/stamp-changelog.sh" 2026.1112.3 "$dir/c.md")
grep -q '^## \[2026.1112.3\] - ' "$dir/c.md" || { echo "heading not stamped" >&2; exit 1; }
! grep -q '^## \[Unreleased\]' "$dir/c.md" || { echo "Unreleased left behind" >&2; exit 1; }
printf '%s' "$notes" | grep -q 'A thing.' || { echo "notes missing" >&2; exit 1; }
! printf '%s' "$notes" | grep -q 'Older.' || { echo "notes ran into the previous release" >&2; exit 1; }

printf '# Changelog\n\n## [Unreleased]\n\n## [2026.1101.0] - 2026-11-01\n- Older.\n' > "$dir/e.md"
[ "$("$here/stamp-changelog.sh" 2026.1112.4 "$dir/e.md")" = "No user-visible changes." ] || { echo "empty section" >&2; exit 1; }

printf '# Changelog\n' > "$dir/m.md"
if "$here/stamp-changelog.sh" 2026.1112.5 "$dir/m.md" 2>/dev/null; then
  echo "a missing heading passed" >&2; exit 1
fi
echo "stamp-changelog: ok"
