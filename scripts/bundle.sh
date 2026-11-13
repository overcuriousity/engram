#!/bin/sh
# Builds the offline release bundles from the binaries in DIST/bin/bin-<target>/engram:
# one archive per target (binary, installer, unit, nginx, config, licences, SHA256SUMS) and a vendored source archive.
# Usage: bundle.sh <version> <dist> [target...]
set -eu

version=$1
dist=$2
shift 2
[ $# -gt 0 ] || set -- x86_64-unknown-linux-musl aarch64-unknown-linux-gnu
out="$dist/out"
mkdir -p "$out"

if [ -z "${SKIP_SOURCE:-}" ]; then
  mkdir -p "$dist/src/.cargo"
  git archive --format=tar HEAD | tar -C "$dist/src" -xf -
  # The printed source replacement points at the build path; inside the archive it is ./vendor.
  cargo vendor --locked --versioned-dirs "$dist/src/vendor" | sed 's|^directory = .*|directory = "vendor"|' > "$dist/src/.cargo/config.toml"
  tar -C "$dist" -czf "$out/engram-$version-src-vendored.tar.gz" --transform "s|^src|engram-$version|" src
fi

if [ -f THIRD-PARTY-LICENSES ]; then
  cp THIRD-PARTY-LICENSES "$dist/THIRD-PARTY-LICENSES"
else
  cargo about generate --locked about.hbs > "$dist/THIRD-PARTY-LICENSES"
fi

for target in "$@"; do
  name="engram-$version-$target"
  dir="$dist/$name"
  mkdir -p "$dir"
  install -m 0755 "$dist/bin/bin-$target/engram" "$dir/engram"
  cp deploy/engram.service deploy/nginx.conf AIRGAP.md config.example.toml cli.example.toml \
     CHANGELOG.md LICENSE-MIT LICENSE-APACHE install.sh "$dir/"
  cp "$dist/THIRD-PARTY-LICENSES" "$dir/THIRD-PARTY-LICENSES"
  (cd "$dir" && sha256sum -- * | grep -v ' SHA256SUMS$' > SHA256SUMS)
  tar -C "$dist" -czf "$out/$name-offline.tar.gz" "$name"
done
cp install.sh "$out/install.sh"
(cd "$out" && sha256sum -- *.tar.gz install.sh > SHA256SUMS)
