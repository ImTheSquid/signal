#!/usr/bin/env bash
# Build the daemon on the Deck, in a container. Run this on the Deck.
#
# The host has no toolchain and cannot be given one that survives a SteamOS
# update, so the container is the toolchain. Only the finished binary runs on the
# host, where it needs nothing but glibc and libasound.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
crate=$(dirname "$here")
image=audio-bridge-builder
# Outside the crate: a bind mount the caller already owns, so nothing depends on
# how podman initialises the ownership of a named volume.
cargo_home=${CARGO_HOME_DECK:-$HOME/.cache/audio-bridge-cargo}

mkdir -p "$cargo_home"
podman build -t "$image" "$here"

# keep-id so the target dir and the binary come out owned by the caller rather
# than root. --locked because the tree is synced from another machine: a lock
# that cannot resolve should fail loudly here, not be silently rewritten.
podman run --rm \
  --userns=keep-id \
  -v "$crate:/crate" \
  -v "$cargo_home:/cargo" \
  -e CARGO_HOME=/cargo \
  -e CARGO_TARGET_DIR=/crate/target-steamos \
  -w /crate \
  "$image" cargo build --release --locked

echo "built $crate/target-steamos/release/audio-bridge"
