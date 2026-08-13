#!/usr/bin/env bash
# Start the daemon as a user service, so it outlives the ssh session that started
# it. Run this on the Deck, or over ssh. Arguments are passed through:
#
#   run.sh --offset -30 --host 192.168.1.255
#
# SteamOS sets logind's KillUserProcesses=True, so everything in a session's
# cgroup dies at logout — a backgrounded process and a tmux server alike. A unit
# under the user manager is what actually survives, and it also restarts the
# daemon if it fails.
set -euo pipefail

unit=${UNIT:-audio-bridge}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# Lingering keeps the user manager alive with nobody logged in, which is what the
# unit hangs off. Allowed for one's own user through polkit, so no root.
loginctl enable-linger

if systemctl --user is-active --quiet "$unit"; then
    echo "already running; stop it with: systemctl --user stop $unit" >&2
    exit 1
fi

# Transient rather than an installed unit file: the arguments above belong to the
# invocation, and there is no second copy of them to drift. --collect so a failed
# unit does not linger and block the next start.
systemd-run --user \
    --unit="$unit" \
    --collect \
    --property=Restart=on-failure \
    --property=RestartSec=2 \
    "$here/launch.sh" "$@" >/dev/null

sleep 2
if ! systemctl --user is-active --quiet "$unit"; then
    echo "the daemon did not stay up:" >&2
    journalctl --user -u "$unit" -n 20 --no-pager >&2
    exit 1
fi

# Startup is the only place a wrong input is cheap to catch. The daemon reports
# the PCM it opened, which on Linux is an endpoint name and says nothing about
# which node it landed on — so ask the graph what its ports are attached to.
links=$(pw-link -l 2>/dev/null | grep -B1 -A1 -F 'alsa_capture.audio-bridge' || true)
if [[ $links == *Focusrite* ]]; then
    echo "$links" | sed 's/^/  /'
else
    echo "warning: the daemon is not linked to the interface" >&2
    echo "         check with: pw-link -l" >&2
fi

echo
echo "watch it:  journalctl --user -u $unit -f"
echo "stop it:   systemctl --user stop $unit"
echo
echo "for the live meter instead, run it in a terminal: deck/launch.sh"
