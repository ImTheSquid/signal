#!/usr/bin/env bash
# Push the crate to the Deck and build it there. Run this on the dev machine.
#
# rsync rather than a git remote so an unpushed working tree is what gets built —
# the Deck is a target, not a place work is done.
set -euo pipefail

host=${DECK_HOST:-steamdeck}
dest=${DECK_DIR:-audio-bridge}
crate=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# Both target dirs are excluded, which also protects the Deck's own build output
# from --delete: rsync does not delete what it was told to ignore.
rsync -a --delete \
  --exclude target \
  --exclude target-steamos \
  "$crate/" "$host:$dest/"

ssh "$host" "$dest/deck/build.sh"
