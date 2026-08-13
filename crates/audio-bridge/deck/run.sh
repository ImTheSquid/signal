#!/usr/bin/env bash
# Launch the daemon on the Deck. Run this on the Deck, or over ssh.
#
# Arguments are passed through, so: run.sh --offset -30 --host 192.168.1.255
set -euo pipefail

session=${SESSION:-audio-bridge}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
bin=$(dirname "$here")/target-steamos/release/audio-bridge
# The PCM the daemon defaults to on Linux; see capture::DEFAULT_DEVICE.
device=pipewire

[[ -x $bin ]] || { echo "no binary at $bin — run deck/build.sh first" >&2; exit 1; }

if tmux has-session -t "$session" 2>/dev/null; then
    echo "already running; attach with: tmux attach -t $session" >&2
    echo "or stop it with: tmux kill-session -t $session" >&2
    exit 1
fi

card=$(pactl list short cards | awk '/Focusrite/ {print $2; exit}')
[[ -n $card ]] || { echo "no Focusrite card — is the interface plugged in?" >&2; exit 1; }

# The interface has to be on the pro-audio profile: the default HiFi profile
# splits the two inputs into separate mono sources, and the tracker wants the
# pair as one stereo node so the downmix and the polarity guard see both legs.
#
# Matching the raw node and not the `alsa_loopback_device.*` one SteamOS layers
# on top: same audio, one less hop of buffering.
scarlett() {
    pactl list short sources | awk '$2 ~ /^alsa_input\..*Focusrite.*pro-input/ {print $2; exit}'
}
node=$(scarlett)
if [[ -z $node ]]; then
    echo "switching $card to pro-audio"
    pactl set-card-profile "$card" pro-audio
    for _ in $(seq 20); do
        node=$(scarlett) && [[ -n $node ]] && break
        sleep 0.25
    done
fi
if [[ -z $node ]]; then
    echo "no pro-audio input node appeared. sources:" >&2
    pactl list short sources >&2
    exit 1
fi
echo "capturing from $node"

# Pre-flight, because a daemon that dies inside tmux takes its own error message
# with it: the pane is gone the moment the process is. This catches the two that
# actually happen — a binary that will not load, and a missing PCM.
if ! "$bin" --list-devices 2>/dev/null | awk -v want="$device" '$1 == want {found = 1} END {exit !found}'; then
    echo "the daemon does not see a \"$device\" input. it reports:" >&2
    "$bin" --list-devices >&2
    exit 1
fi

# PIPEWIRE_NODE is what stops capture following the system default source, which
# moves the moment anything else audio-shaped is plugged in. By name rather than
# id, so it still resolves if the stream is rebuilt after a replug.
#
# systemd-inhibit because an idle Deck suspends, and a suspended Deck is a light
# that stops mid-set. This holds the lock only while the daemon runs.
inner=$(printf '%q ' \
    systemd-inhibit --what=idle:sleep --who=audio-bridge \
    --why="driving the traffic light" \
    "$bin" "$@")
tmux new-session -d -s "$session" "PIPEWIRE_NODE=$(printf '%q' "$node") exec $inner"

sleep 2
if ! tmux has-session -t "$session" 2>/dev/null; then
    echo "the daemon exited immediately. run it in the foreground to see why:" >&2
    echo "  PIPEWIRE_NODE=$node $bin $*" >&2
    exit 1
fi

# Startup is the only place a wrong link is cheap to catch. The daemon reports
# the PCM it opened, which on Linux is an endpoint name and says nothing about
# which node it landed on — so ask the interface's own ports what is attached to
# them instead.
links=$(pw-link -l 2>/dev/null | grep -A3 -F "$node:capture_AUX" || true)
if [[ $links == *'->'* ]]; then
    echo "$links" | sed 's/^/  /'
else
    echo "warning: nothing is linked to $node yet" >&2
    echo "         check with: pw-link -l" >&2
fi

echo
echo "watch it:  tmux attach -t $session          (detach: ctrl-b d)"
echo "stop it:   tmux kill-session -t $session"
