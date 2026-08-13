#!/bin/bash
# Turn WiFi power save off. Runs as root, reached through sudo with no password —
# see sudoers.audio-bridge for what makes that safe.
#
# Takes no arguments on purpose. Everything it touches is written down here, so
# the sudoers entry grants one fixed action rather than anything assembled from
# input. Absolute paths for the same reason: PATH is not to be trusted by a
# script that runs as root.
set -euo pipefail

dev=$(/usr/bin/iw dev | /usr/bin/awk '/Interface/ {print $2; exit}')
[[ -n $dev ]] || { echo "no wireless interface" >&2; exit 1; }

/usr/bin/iw dev "$dev" set power_save off
echo "$dev: $(/usr/bin/iw dev "$dev" get power_save)"
