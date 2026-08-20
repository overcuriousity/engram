#!/bin/bash
# Build on a 2 GB box with no swap.
#
# Two things blow the memory budget here, both at link time: dependencies
# compiled with full debuginfo (the default `dev` profile), and GNU ld holding
# the whole thing in memory at once. Dropping debuginfo and linking with the
# lld that ships inside the toolchain fits the link into what is left after
# the editor and the service.
set -euo pipefail
LLD="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld"
export RUSTFLAGS="-C link-arg=-B$LLD -C link-arg=-fuse-ld=lld"
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
exec cargo "$@" -j 1
