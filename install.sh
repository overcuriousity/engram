#!/bin/sh
# Fetch the newest released engram binary for this machine and put it on PATH; POSIX sh, curl, tar, sha256.
# Offline: install.sh --archive engram-<version>-<target>-offline.tar.gz [--system] [--prefix DIR]
# Environment: ENGRAM_INSTALL_DIR (default: see `pick_dir`), ENGRAM_VERSION (a tag; default: newest).
set -eu

REPO=overcuriousity/engram

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

need() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is needed and was not found"
}

# Only the two released targets: a wrong-architecture binary fails at exec with no useful message.
pick_target() {
  case "$(uname -s)" in
    Linux) ;;
    *) die "there are no released binaries for $(uname -s); build from source — see the README" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) echo x86_64-unknown-linux-musl ;;
    aarch64 | arm64) echo aarch64-unknown-linux-gnu ;;
    *) die "there is no released binary for $(uname -m); build from source — see the README" ;;
  esac
}

# `/releases/latest` skips pre-releases; the list is newest first. sed, not jq: a fresh server has none.
# shellcheck disable=SC2020 # both separators become newlines on purpose
newest_tag() {
  curl -fsSL -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$REPO/releases?per_page=10" |
    tr ',{' '\n\n' |
    sed -n 's/.*"tag_name" *: *"\([^"]*\)".*/\1/p' |
    head -n 1
}

# Never sudo: a piped script cannot read a password prompt.
pick_dir() {
  if [ -n "${ENGRAM_INSTALL_DIR:-}" ]; then
    echo "$ENGRAM_INSTALL_DIR"
  elif [ -w /usr/local/bin ]; then
    echo /usr/local/bin
  else
    echo "$HOME/.local/bin"
  fi
}

verify() {
  # `sha256sum` on Linux, `shasum` where coreutils is not what is installed.
  if command -v sha256sum >/dev/null 2>&1; then
    grep " \./$1\$" SHA256SUMS | sha256sum -c - >/dev/null
  elif command -v shasum >/dev/null 2>&1; then
    grep " \./$1\$" SHA256SUMS | shasum -a 256 -c - >/dev/null
  else
    die "no sha256sum or shasum, so the download cannot be checked; install one or download by hand"
  fi
}

# An offline bundle: no network, checksums verified, ~/.local by default, never sudo.
install_archive() {
  archive=$1 system=$2 prefix=$3
  [ -f "$archive" ] || die "archive not found: $archive"
  if [ "$system" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then
    die "run as root for --system; this script never calls sudo"
  fi
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT INT TERM
  tar -C "$work" -xzf "$archive"
  set -- "$work"/*/
  [ $# -eq 1 ] && [ -d "$1" ] || die "the archive should contain exactly one directory"
  cd "${1%/}"
  if ! sha256sum -c --quiet SHA256SUMS > "$work/check.log" 2>&1; then
    bad=$(grep -v ': OK$' "$work/check.log" | head -n 1 | cut -d: -f1)
    die "checksum mismatch: ${bad:-SHA256SUMS}; the bundle is damaged or was altered"
  fi
  mkdir -p "$prefix/bin"
  install -m 0755 engram "$prefix/bin/engram"
  say "installed $prefix/bin/engram"
  if [ "$system" -eq 1 ]; then
    install -m 0644 engram.service /etc/systemd/system/engram.service
    mkdir -p /etc/engram
    if [ ! -f /etc/engram/config.toml ]; then
      install -m 0644 config.example.toml /etc/engram/config.toml
      say "wrote /etc/engram/config.toml; set server.public_url before starting"
    fi
    say "start it with: systemctl daemon-reload && systemctl enable --now engram"
  fi
  doc="$prefix/share/engram"
  mkdir -p "$doc"
  for f in nginx.conf AIRGAP.md THIRD-PARTY-LICENSES CHANGELOG.md config.example.toml; do
    [ -f "$f" ] && install -m 0644 "$f" "$doc/$f"
  done
  say "reverse proxy template: $doc/nginx.conf"
}

archive="" system=0 prefix="${HOME:-}/.local"
while [ $# -gt 0 ]; do
  case "$1" in
    --archive) [ $# -ge 2 ] || die "--archive needs a file"; archive=$2; shift 2 ;;
    --system) system=1; prefix=/usr/local; shift ;;
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; prefix=$2; shift 2 ;;
    *) die "unknown option: $1" ;;
  esac
done
if [ -n "$archive" ]; then
  install_archive "$archive" "$system" "$prefix"
  exit 0
fi

need curl
need tar

target=$(pick_target)
tag=${ENGRAM_VERSION:-$(newest_tag)}
[ -n "$tag" ] || die "could not find a release; check https://github.com/$REPO/releases"
version=${tag#v}
archive="engram-${version}-${target}-offline.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"

tmp=$(mktemp -d)
# Including on failure: no half-downloaded archive left in /tmp.
trap 'rm -rf "$tmp"' EXIT INT TERM

say "engram $tag ($target)"
curl -fsSL -o "$tmp/$archive" "$base/$archive" ||
  die "no $archive in $tag — see https://github.com/$REPO/releases/tag/$tag"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" ||
  die "$tag publishes no SHA256SUMS, so this download cannot be checked"

# Refused before anything is unpacked.
( cd "$tmp" && verify "$archive" ) ||
  die "$archive does not match the published checksum; nothing was installed"

tar -xzf "$tmp/$archive" -C "$tmp"
binary="$tmp/engram-${version}-${target}/engram"
[ -f "$binary" ] || die "the archive did not contain the binary it should"

dir=$(pick_dir)
mkdir -p "$dir" || die "cannot create $dir; set ENGRAM_INSTALL_DIR to somewhere writable"
# Moved into place, so an interrupted install never leaves a truncated binary.
cp "$binary" "$dir/.engram.new" || die "cannot write to $dir; set ENGRAM_INSTALL_DIR to somewhere writable"
chmod 755 "$dir/.engram.new"
mv "$dir/.engram.new" "$dir/engram"

say "installed $dir/engram"

case ":${PATH}:" in
  *":$dir:"*) ;;
  # Said, not fixed: a piped script does not edit shell profiles.
  *) say "note: $dir is not on your PATH" ;;
esac

cat <<EOF

Next:
  engram --hash-password 'your password'   # paste the hash into config.toml
  engram --print-config                    # what engram resolved, secrets redacted
  engram                                   # needs Qdrant answering on its REST port

config.example.toml, cli.example.toml and deploy/engram.service ship in the
same archive, at https://github.com/$REPO/releases/tag/$tag
EOF
