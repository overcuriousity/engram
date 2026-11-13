#!/bin/sh
# Builds a bundle from a binary, installs it offline into a temp prefix, then checks a tampered bundle is refused.
# Usage: test-install.sh [path/to/engram]   (default: target/debug/engram)
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-$here/target/debug/engram}
[ -x "$bin" ] || { echo "no binary at $bin; cargo build first" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

mkdir -p "$work/dist/bin/bin-test"
cp "$bin" "$work/dist/bin/bin-test/engram"
[ -f "$here/AIRGAP.md" ] || fail "AIRGAP.md is missing"
(cd "$here" && SKIP_SOURCE=1 sh scripts/bundle.sh 0.0.0 "$work/dist" test >/dev/null)
bundle="$work/dist/out/engram-0.0.0-test-offline.tar.gz"
[ -f "$bundle" ] || fail "no bundle was built"

offline() {
  if command -v unshare >/dev/null 2>&1 && unshare -rn true 2>/dev/null; then
    unshare -rn "$@"
  else
    env PATH=/usr/bin:/bin "$@"
  fi
}

HOME="$work/home" offline sh "$here/install.sh" --archive "$bundle" > "$work/out" 2>&1 || fail "install failed: $(cat "$work/out")"
"$work/home/.local/bin/engram" --version > /dev/null || fail "the installed binary does not run"
grep -q nginx.conf "$work/out" || fail "the install did not point at the nginx template"

mkdir -p "$work/bad"
tar -C "$work/bad" -xzf "$bundle"
printf 'x' >> "$work/bad/engram-0.0.0-test/config.example.toml"
tar -C "$work/bad" -czf "$work/bad.tar.gz" engram-0.0.0-test
if HOME="$work/home2" offline sh "$here/install.sh" --archive "$work/bad.tar.gz" > "$work/out" 2>&1; then
  fail "a tampered bundle was installed"
fi
grep -q "checksum mismatch" "$work/out" || fail "no checksum message: $(cat "$work/out")"
echo "install.sh --archive: all checks passed"
