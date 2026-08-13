# Running the bridge on a Steam Deck

The Deck pulls the master output in through a Focusrite Scarlett and has to keep working while
Audacity records the same inputs.

```sh
deck/sync.sh                       # from the dev machine: rsync, then build on the Deck
ssh steamdeck audio-bridge/deck/run.sh --offset -30
```

`sync.sh` takes `DECK_HOST` (default `steamdeck`) and `DECK_DIR` (default `audio-bridge`).
Arguments to `run.sh` are passed through to the daemon.

## Why it is built in a container

SteamOS has a read-only rootfs and ships no compiler, no pkg-config and no ALSA headers, and
anything installed with `steamos-readonly disable` is lost at the next update. So the toolchain
is a container image and only the finished binary runs on the host, where it needs nothing but
glibc and libasound.

The image is Debian bookworm rather than an Arch base on purpose: the binary loads against the
host's glibc 2.41, and linking against bookworm's older 2.36 is the direction that works. It
carries no clang, because the aubio FFI bindings ship prebuilt for `x86_64-linux-gnu` and this
target never runs bindgen — the macOS build is the one that needs libclang.

## Sharing the interface with Audacity

cpal's Linux backend is ALSA, and on the Deck `libasound` is PipeWire's ALSA plugin, so the
daemon is a PipeWire client like anything else and the interface is not owned by whoever opened
it first. Audacity (the flatpak) reaches the same node through the Pulse socket.

Two things `run.sh` sets up for that to hold:

- **The `pro-audio` card profile.** The default `HiFi` profile splits the Scarlett's two inputs
  into separate mono sources, and the tracker wants the pair as one stereo node so the downmix
  and the polarity guard see both legs.
- **`PIPEWIRE_NODE`**, pinned to the Scarlett by name. `--device` names a PipeWire endpoint, not
  a device, so without this capture follows the system default source — which moves the moment
  anything else audio-shaped is plugged in. `run.sh` prints the resulting links so a wrong one
  is caught at startup rather than mid-set.

## Two things that will stop it

An idle Deck suspends, and a suspended Deck is a light that dies mid-set, so the daemon runs
under `systemd-inhibit --what=idle:sleep`. That holds only while it runs, and it does not
survive the lid being closed or the power button.

The daemon runs inside tmux so it outlives the ssh session. It also means the status meter is
still there to look at:

```sh
ssh steamdeck -t tmux attach -t audio-bridge      # detach with ctrl-b d
ssh steamdeck tmux kill-session -t audio-bridge
```
