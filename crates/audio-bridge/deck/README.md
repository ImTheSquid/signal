# Running the bridge on a Steam Deck

The Deck pulls the master output in through a Focusrite Scarlett and has to keep working while
Audacity records the same inputs.

```sh
deck/sync.sh                       # from the dev machine: rsync, then build on the Deck
ssh steamdeck audio-bridge/deck/run.sh --offset -30
```

`sync.sh` takes `DECK_HOST` (default `steamdeck`) and `DECK_DIR` (default `audio-bridge`).
Arguments to `run.sh` are passed through to the daemon.

`run.sh` starts it as a service and reports to the journal. To watch the live meter instead —
which is what calibrating `--offset` wants — run `deck/launch.sh` in a terminal: same interface
pinning, same inhibit, just in the foreground.

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

Measured: the daemon, `arecord` and `parec` captured the same node at once, with no stream
errors and no xruns on any of them.

Two things `launch.sh` sets up for that to hold:

- **The `pro-audio` card profile.** The default `HiFi` profile splits the Scarlett's two inputs
  into separate mono sources, and the tracker wants the pair as one stereo node so the downmix
  and the polarity guard see both legs.
- **The `scarlett` PCM** from `asound.conf`, selected with `--device` and pointed at the node by
  `AUDIO_BRIDGE_NODE`. Without it capture follows PipeWire's default source, which moves the
  moment anything else audio-shaped is plugged in.

`PIPEWIRE_NODE` does nothing here — it belongs to `pw-cat` and friends, not to the ALSA plugin,
which routes by its own `capture_node` key. That is worth knowing because when the target does
not resolve, nothing fails: the plugin quietly falls back to the default source. Hence the PCM,
and hence `run.sh` printing the links it actually got.

## What will stop it

**Sleep.** An idle Deck suspends — measured at 8 minutes, `PM: suspend entry (deep)` — and a
suspended Deck is a light that dies mid-set. Three things stand against it, in order of how
much they cover:

```sh
cp deck/keep-awake.service ~/.config/systemd/user/
systemctl --user enable --now keep-awake      # no root, survives reboot
```

That holds an idle inhibitor permanently, covering the gaps the daemon cannot: soundcheck, a
stopped daemon, the walk between sets. The daemon holds one of its own while it runs, and
Audacity holds one while it plays or records — worth confirming with `systemd-inhibit --list`,
since it only helps while a stream is actually active.

None of them covers the lid or the power button; an inhibitor blocks the idle timer, not a
deliberate suspend. `systemctl --user disable --now keep-awake` is the whole of undoing it.

**WiFi power save**, which is on by default, but read the numbers before caring: measured
outbound — the only direction the light uses — 750/750 packets, 0% loss, p50 gap 19.7ms against
20ms nominal. Inbound was 12% loss with 780ms spikes, which costs ssh and nothing else. If you
want it off anyway, `deck/sudoers.audio-bridge` installs a digest-pinned NOPASSWD entry for
`deck/wifi-powersave-off.sh` and `launch.sh` then uses it automatically; without that file
nothing asks for a password. `nmcli` cannot do it from an ssh session — there is no polkit agent
to authenticate against — but KDE's network settings can, from the Deck's own screen.

**Logout.** SteamOS sets logind's `KillUserProcesses=True`, so a backgrounded process or a tmux
server dies with the ssh session that started it. That is why the daemon is a transient user
unit and why `run.sh` enables lingering, which needs no root.

```sh
journalctl --user -u audio-bridge -f      # one status line every 10s
systemctl --user stop audio-bridge
```
