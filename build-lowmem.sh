#!/bin/bash
# Build on a 2 GB box with no swap: no dependency debuginfo, and the toolchain's lld instead of GNU ld.
set -euo pipefail
LLD="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld"
export RUSTFLAGS="-C link-arg=-B$LLD -C link-arg=-fuse-ld=lld"
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
exec cargo "$@" -j 1
