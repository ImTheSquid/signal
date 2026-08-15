#!/usr/bin/env bash
# Resolve the interface and run the daemon in the foreground.
#
# This is both the service's ExecStart and the way to run it by hand: in a
# terminal the status meter draws live, and with no terminal it logs a line every
# ten seconds instead. Arguments are passed through to the daemon.
#
# Everything this script says goes to stderr, because stdout is the --probe TSV.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
bin=$(dirname "$here")/target-steamos/release/audio-bridge

[[ -x $bin ]] || { echo "no binary at $bin — run deck/build.sh first" >&2; exit 1; }

card=$(pactl list short cards | awk '/Focusrite/ {print $2; exit}')
[[ -n $card ]] || { echo "no Focusrite card — is the interface plugged in?" >&2; exit 1; }

# The interface has to be on the pro-audio profile: the default HiFi profile
# splits the two inputs into separate mono sources, and the tracker wants the
# pair as one stereo node so the downmix and the polarity guard see both legs.
#
# This resolves to the `alsa_loopback_device.*` node SteamOS layers over the raw
# capture device, because that is the only one exposed as a source and so the one
# every other recorder gets. Taking the same path as they do is worth more than
# the hop it costs, which is fixed latency and so is what --offset absorbs.
scarlett() {
    pactl list short sources | awk '$2 ~ /Focusrite.*pro-input/ {print $2; exit}'
}
node=$(scarlett)
if [[ -z $node ]]; then
    echo "switching $card to pro-audio" >&2
    pactl set-card-profile "$card" pro-audio
    for _ in $(seq 20); do
        node=$(scarlett)
        [[ -n $node ]] && break
        sleep 0.25
    done
fi
if [[ -z $node ]]; then
    echo "no pro-audio input node appeared. sources:" >&2
    pactl list short sources >&2
    exit 1
fi
echo "capturing from $node" >&2

# Power save costs the inbound path far more than the outbound one the light
# actually uses, so this is optional and stays quiet when it is not set up: with
# the sudoers entry installed it needs no password, and without it nothing here
# asks for one. See sudoers.audio-bridge.
if [[ -f /etc/sudoers.d/audio-bridge ]]; then
    sudo -n "$here/wifi-powersave-off.sh" >&2 || echo "wifi power save left alone" >&2
fi

# The `scarlett` PCM in deck/asound.conf reads AUDIO_BRIDGE_NODE at open, and is
# what stops capture following the system default source — which moves the moment
# anything else audio-shaped is plugged in. By name rather than id, so it still
# resolves if the stream is rebuilt after a replug.
#
# --device first so a caller can still override it.
#
# systemd-inhibit because an idle Deck suspends, and a suspended Deck is a light
# that stops mid-set. The lock is held only while the daemon runs.
exec env \
    ALSA_CONFIG_PATH="$here/asound.conf" \
    AUDIO_BRIDGE_NODE="$node" \
    systemd-inhibit \
        --what=idle:sleep \
        --who=audio-bridge \
        --why="driving the traffic light" \
        "$bin" --device scarlett "$@"
