#!/bin/sh
# Fetch the newest released engram binary for this machine and put it on PATH.
#
# POSIX sh, and nothing beyond curl, tar and a sha256 tool: the whole point of
# a one-line installer is that it runs on the box you are already logged into,
# which may be an Alpine container with no bash and no jq.
#
# Environment:
#   ENGRAM_INSTALL_DIR   where to put the binary (default: see `pick_dir`)
#   ENGRAM_VERSION       a specific tag, e.g. v2026.827.0 (default: the newest)
set -eu

REPO=overcuriousity/engram

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

need() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is needed and was not found"
}

# The two targets the release workflow builds, and no guessing beyond them: a
# binary for the wrong architecture fails at exec with a message that says
# nothing useful, so an unknown machine is told so here instead.
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

# Releases are cut as pre-releases while the version is under 1.0, and
# `/releases/latest` skips those — it would answer 404 here forever. The list
# endpoint comes back newest first; drafts are not visible to an anonymous
# caller, so the first entry is the newest published release.
#
# Read with sed rather than jq, which is not on a fresh server. Splitting on
# `,` and `{` first means this does not depend on the API pretty-printing.
newest_tag() {
  curl -fsSL -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$REPO/releases?per_page=10" |
    tr ',{' '\n\n' |
    sed -n 's/.*"tag_name" *: *"\([^"]*\)".*/\1/p' |
    head -n 1
}

# Somewhere on PATH that this user can actually write to, without reaching for
# sudo: a piped script silently escalating is a surprise, and one that prompts
# for a password mid-pipe cannot read the answer anyway.
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

need curl
need tar

target=$(pick_target)
tag=${ENGRAM_VERSION:-$(newest_tag)}
[ -n "$tag" ] || die "could not find a release; check https://github.com/$REPO/releases"
version=${tag#v}
archive="engram-${version}-${target}.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"

tmp=$(mktemp -d)
# Including on the failure paths above: a half-downloaded archive left in /tmp
# is the one piece of litter an installer has no excuse for.
trap 'rm -rf "$tmp"' EXIT INT TERM

say "engram $tag ($target)"
curl -fsSL -o "$tmp/$archive" "$base/$archive" ||
  die "no $archive in $tag — see https://github.com/$REPO/releases/tag/$tag"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" ||
  die "$tag publishes no SHA256SUMS, so this download cannot be checked"

# Checked before anything is unpacked, and the archive is refused rather than
# reported: an installer that carries on after a bad checksum has none.
( cd "$tmp" && verify "$archive" ) ||
  die "$archive does not match the published checksum; nothing was installed"

tar -xzf "$tmp/$archive" -C "$tmp"
binary="$tmp/engram-${version}-${target}/engram"
[ -f "$binary" ] || die "the archive did not contain the binary it should"

dir=$(pick_dir)
mkdir -p "$dir" || die "cannot create $dir; set ENGRAM_INSTALL_DIR to somewhere writable"
# Written under a temporary name and moved into place, so an install that is
# interrupted never leaves a truncated binary where a working one was.
cp "$binary" "$dir/.engram.new" || die "cannot write to $dir; set ENGRAM_INSTALL_DIR to somewhere writable"
chmod 755 "$dir/.engram.new"
mv "$dir/.engram.new" "$dir/engram"

say "installed $dir/engram"

case ":${PATH}:" in
  *":$dir:"*) ;;
  # Said rather than fixed: editing somebody's shell profile from inside a
  # piped script is not this program's business.
  *) say "note: $dir is not on your PATH" ;;
esac

cat <<EOF

Next:
  engram --hash-password 'your password'   # paste the hash into config.toml
  engram --print-config                    # what engram resolved, secrets redacted
  engram                                   # needs Qdrant answering on its REST port

config.example.toml, cli.example.toml and engram.service ship in the same
archive, at https://github.com/$REPO/releases/tag/$tag
EOF
