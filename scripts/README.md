# scripts

Helpers that aren't part of a build.

## `run-script.sh`

Runs a script on the light for as long as you leave it running. Use this rather than a one-off
`curl`:

```sh
scripts/run-script.sh                      # runs follow.rhai until Ctrl-C
scripts/run-script.sh scripts/other.rhai
LOCK_S=120 scripts/run-script.sh           # shorter cycle
COMPONENTS=array scripts/run-script.sh scripts/pulse.rhai
```

`COMPONENTS` is the rhai standard-library surface the script is given, default `array,math`.
Declaring less leaves more heap for the script itself; under-declaring is not caught
server-side and fails at the call, on the light.

**Why a loop is necessary.** A job's TTL is fixed when it is submitted, from the lock's
*remaining* time — re-acquiring the lock does **not** extend a script already running
(`locks.ts` stores the job under `PTTL lock`, and the device enforces that `ttl_ms` locally).
Holding the light for a whole set therefore means re-acquiring *and* resubmitting on a cycle,
which is what this does, resubmitting `MARGIN_S` before each TTL expires. Ctrl-C releases the
lock so nobody is left blocked.

The API key is read from, first match winning:

1. `$TRAFFIC_LIGHT_TOKEN`
2. `--token-file PATH` (a file containing just the token)
3. `~/.config/traffic-light/token` — the normal place; `chmod 600` it
4. `~/.claude/traffic-light.json` (a JSON file with a `token` field)

Two implementation notes, both learned the hard way:

- Sleeps are `sleep N & wait` rather than `sleep N`. Bash defers a trap until the running
  command finishes, so a bare `sleep 285` would swallow Ctrl-C for minutes.
- The lock/submit calls capture the HTTP status, so a legitimate `409` (someone else holds the
  light) is distinguished from a network failure without issuing the request twice.

## `follow.rhai`

The Rhai script the light runs to follow rekordbox lighting via **dmx-bridge**. It needs no
firmware change, so tune it freely.

**The file carries no comments and this section is its documentation.** Every byte is a heap
transient on a device that reboots on a failed allocation, and the explanation is more useful
here than inline; the design rationale below is the comment block that used to be at the top.

`run-script.sh` is the easy way to run it; by hand it is:

```sh
BASE=https://signal.jackhogan.me
AUTH="Authorization: Bearer tl_<id>_<secret>"
curl -s -X POST $BASE/v1/lock -H "$AUTH" -H 'Content-Type: application/json' -d '{"duration_s":300}'
curl -s -X POST $BASE/v1/script -H "$AUTH" -H 'Content-Type: application/json' \
  -d "$(python3 -c "import json;print(json.dumps({'script': open('scripts/follow.rhai').read()}))")"
```

### Why it isn't a plain threshold

Measured over 266s of a real set, with the fixture patched as a 3-channel RGB par on DMX 1-3:

| | share of time |
|---|---|
| any channel > 0 | 27.9% |
| any channel >= 128 | 15.4% |

Peaks were `R=255 G=131 B=255`, median non-zero level 118. Three consequences:

- **A fixed `>= 128` cut can never light green.** It peaks at 131, and a cut taken against the
  frame maximum needed 204. Each channel therefore gets its **own** running peak with slow
  decay, so a channel the venue only ever drives to half still uses its whole lamp.
- **Absolute levels carry nothing.** `[3,4,6]` is a typical frame. Everything works on
  normalised levels and on *change*.
- **The output is dark 72% of the time, and the light must not be.** So the pattern is
  generated, and DMX steers it. Density is no longer bounded by rekordbox's duty cycle,
  which is what made every earlier version look sparse.
- **Whether anything moves at beat rate depends on the content.** An early ambient capture
  showed nothing faster than 0.5 rises/s and the design concluded rekordbox never sends
  rhythm; a loop measured 2026-08-04 showed the MH dimmer pumping full-scale at **13
  changes/s**, par R/B fading at ~9/s, and pan sweeping fast (10.6/s). Both are real. So
  the rhythm is **followed when the stream carries one and generated when it doesn't** —
  the onset detector feeds a beat estimator that free-runs between usable onsets.

  Still true from the early capture: two channels are named `Strobe` and rekordbox drives
  neither — the moving head's `Strobe` and the strobe fixture's combined `Dimmer/Strobe` —
  while driving a plain `Dimmer`. A fixture whose intensity is a combined channel sits dark.

  Each DMX channel is used for what it actually carries:

  | signal | from | used for |
  |---|---|---|
  | colour | par R/G/B (1-3) | ranking which lamps a look lights, brightest first; a fully dark par rotates the ranking per beat instead of freezing it |
  | energy | MH dimmer (5) | how many lamps light — the width — and, through its onsets, the tempo |
  | phrase | MH pan (7) | a sweep reversal re-rolls the look — gated on traversing 35% of pan's recent range, or fast pan re-rolls it constantly |
  | section | strobe R/G/B (12-14) | a binary gate that *lowers* the width thresholds when hot; when cold it does nothing — a cold gate must never narrow the light, because it measured hot only 0.1% of a real set |
  | timing | seq | how many sent frames a receive gap swallowed — see below, this is what keeps the tempo locked to the music instead of the wifi |

  Absence is not a value: with fewer channels than that, each signal falls back rather than
  reading as zero. A 3-channel stream still works.

### The wire is bursty, and seq is the defence

  Measured on a live loop: the bridge logs 23.2 changed-frames/s, but they arrive in
  clumps — p50 inter-frame gap **0ms**, p95 306ms. The receiver coalesces each clump to
  the newest frame (deliberately — stale lamp states must not replay), so the script sees
  ~5.7 frames/s of a 23fps stream, with energy accumulated across each gap.

  Run naively, the onset detector fires on the clump boundaries: onset intervals cluster
  at the ~300ms network gap and the tempo locks to the wifi (measured: 560ms period
  against 457ms playing). The bridge's `seq` counts every *sent* packet, so the receive
  side can see how much a gap swallowed:

  - flux is normalised per sender frame (`Δe / Δseq`): a musical attack is a large jump
    across few frames, a coalesced fade is the same jump across many;
  - an onset with `Δseq > 3` neither votes on the tempo nor fires the accent — its
    timing is the network's, not the music's;
  - the phase anchor is a snap-or-nudge PLL (snap beyond a quarter period, else 1/4 of
    the error), so per-onset arrival jitter stops smearing the beat grid. Adopting each
    onset's arrival wholesale measured 0.054 on-grid; the PLL measures 0.364.

  The clumping itself is ESP-IDF's default modem power save: the AP buffers unicast
  between beacon wakes. The bridge already runs `WIFI_PS_NONE` for exactly this reason;
  the light's firmware now does too, which removes most of the bunching at the source —
  the seq handling stays, because drops and congestion still happen.

Two AGC references, not one, because they answer different questions. Deciding *which lamp*
wants a **per-channel** peak, so a channel the venue only drives to half still uses its lamp.
Measuring *energy* wants a **single shared** peak across channels: under per-channel
normalisation a dim channel scales to ~1.0, so the max sits near 1.0 whenever anything is lit
and the envelope disappears. Conflating the two cost half the tempo lock (0.469 → 0.229).

### How it works

Per frame: per-channel AGC → normalised levels → the lamp ranking (rotated per beat when
the colour is dead) → energy → seq-normalised rectified flux against an adaptive floor →
onsets. Clean onsets (`Δseq ≤ 3`) are octave-folded into 380-680ms and fed to an
agreement-gated EMA for the period and a snap-or-nudge PLL for the phase. Three
disagreeing intervals in a row are read as a track change and taken as the new tempo.

The pattern renders on a quarter-beat grid. Two things decide what is lit:

- **Which lamps** — the three lamps are ranked by their own normalised colour level, brightest
  first, so a look that lights `k` lamps lights the `k` the music is most in.
- **How many** — width comes from the energy envelope: two lamps whenever DMX is live
  (the floor is deliberate — one lamp reads as broken, and the relay wear it costs is
  budgeted below), three past `FULL_AT`. Rest phases drop to width-1, never to nothing:
  contrast comes from narrowing, not blackouts. Measured on the live-loop stream, writes
  split 8% / 21% / 35% / 34% across 0 / 1 / 2 / 3 lamps.

`look` is re-rolled every `LOOK_BEATS` beats — or at a phrase boundary — and each clean
onset punches all three through briefly as an accent.

| `look` | name | what it does |
|---|---|---|
| 0 | pulse | full width on the front half of the cycle, narrows on the back |
| 1 | chase | a single lamp walking the ranking, half a cycle each |
| 2 | stab | two full-width hits per cycle, narrow between |
| 3 | offbeat | narrow on the downbeat half, full width after — syncopated |
| 4 | swell | width climbs 1 → 2 → 3 across the cycle, then resets |
| 5 | scatter | random width (never zero) and starting lamp, re-rolled every half cycle |

A **stillness watchdog** guarantees a change at least every half cycle: one look's rest phase
running into the next look's rest phase held the lamps for 825ms, and the watchdog cuts the worst
case to 400ms. It fires at most once per quarter and only when the pattern would otherwise be
still, so it costs nothing where the light is already busy.

After `DEAD_MS` with no DMX at all the bridge is assumed gone and the light falls back to a slow
wander. Deliberately neither a red-yellow-green sequence nor all three lamps at 1Hz: the first
would read as an ordinary traffic light, the second is the firmware's fault signal.

Two hardware facts shape the rendering:

- **Quarter beats, never eighths.** At 128 BPM an eighth is 59ms, under the 100ms relay dwell,
  so it cannot be rendered at all. Above ~150 BPM the cycle doubles to half time rather than
  drifting against the track, and below ~86 BPM it halves to double time — `PATTERN_MAX` bounds
  how still the light can get when the tempo estimate runs slow, which on sparse input it does,
  because octave-folding biases toward `BEAT_MAX`.
- **The script gates its own writes on `lamp_dwell_ms()`** instead of letting `set_lights`
  block. A blocked call stalls the loop, and a stalled loop misses DMX frames.

### Verified, not asserted

`crates/script-env/tests/follow.rs` runs this exact file against a synthetic stream on a virtual
clock — 60 simulated seconds in ~0.1s, deterministic. Every past failure here was invisible in
the source and obvious in the output, so the output is what is checked: transition density,
dwell compliance, green actually firing against a lower peak, movement through a 6s blackout,
and no imitation of the firmware's 1Hz fault signal.

Every stream in that file is *synthetic*, including the one built from measurements, so none of
it can disagree with a live set. `FOLLOW_CAPTURE` points the `replays_a_capture` test at a real
recording from `dmxcap.mjs` instead, and prints the lamp timeline the script would have produced
from it — which is what `watch.mjs` records the light actually doing.

```sh
FOLLOW_CAPTURE=$PWD/dmx.txt cargo test -p script-env --test follow -- --nocapture
```

Tempo lock is measured against a control — circular concentration of transition times on the
quarter-beat grid of the tempo playing, versus a grid it must not lock to. The last row is
the case that broke live: 50% of its delivery windows are bunched into 300ms flushes, and
the grid it must beat is the network's:

| stream | on its own grid | on the wrong grid | noise floor |
|---|---|---|---|
| 128 BPM, clean delivery | 0.364 | 0.019 | 0.049 |
| 100 BPM, clean delivery | 0.355 | 0.052 | 0.052 |
| 131.5 BPM, 50% bunched | 0.170 | 0.049 (the burst grid) | 0.052 |

Density, across five streams. The live-loop row replays what a real deck sent on
2026-08-04, through bunched delivery, with a dead-colour stretch:

| stream | transitions/s | longest still gap |
|---|---|---|
| kick on every beat, 41Hz | ~7 | — |
| sparse, mostly dark, 41Hz | 7.3 | 264ms |
| one saturated colour at a time, 10Hz | 8.3 | 260ms |
| measured: par + moving head, 10Hz | 5.7 | 400ms |
| **live loop, bunched delivery** | **7.3** | **260ms (dark ≤ 152ms)** |

**The relay wear is a spent budget, not a ceiling.** The width floor prices at
241 / 262 / 191 ops/min for red / yellow / green on the kick stream and ~172-174 on the
measured one — 6-10 hours of set time per 10^5 electrical operations, a deliberate trade
of relay life for a light that never looks broken. The test now asserts only that the
dwell gate held (nothing past `60000/DWELL_MS` per minute); the per-lamp cost is printed
so a regression is visible, not fatal. Past the dwell gate writes get arbitrated and the
tempo lock halves, so the dwell itself stays.

### Tuning

All of them are `let` bindings at the top of the file, in this order:

| name | effect |
|---|---|
| `WIDE_AT`, `FULL_AT` | energy thresholds for the second and third lamp; lower lights more together (the live floor of two applies regardless) |
| `VOTE_SEQ` | largest `Δseq` whose onset may vote on tempo or fire the accent; larger trusts arrival timing more |
| `COLOUR_DEAD` | normalised colour level below which the ranking rotates instead of freezing |
| `DIMMER_CH`, `PAN_CH`, `HOT_CH` | indices of the energy, phrase, and section channels |
| `PHRASE_MIN` | shortest gap between phrase re-rolls, on top of the 35%-of-range traversal gate |
| `LOOK_BEATS` | beats before a new look is chosen; lower changes character more often |
| `THR_MULT`, `THR_BIAS` | onset sensitivity against the adaptive floor; lower finds more onsets and more accents |
| `PEAK_DECAY` | how fast the AGC ceiling falls, per step; lower adapts quicker to a dimmer track |
| `PEAK_FLOOR` | raw floor under the AGC ceiling, so a dark passage cannot amplify noise |
| `STEP` | decision cadence in ms; kept above the ~41Hz DMX rate |
| `FLUX_SMOOTH`, `THR_SMOOTH` | EMA rates for the flux signal and its floor |
| `REFRACT` | minimum ms between onsets |
| `BEAT_MIN`, `BEAT_MAX` | the octave-folding window, 158 to 88 BPM |
| `PATTERN_MAX` | longest pattern cycle; bounds how still the light can get |
| `BEAT_GAP` | an onset interval longer than this says nothing about tempo |
| `AGREE_PCT` | how far an interval may sit from the estimate and still be folded in |
| `DEAD_MS` | DMX silence before the bridge is assumed gone |

Re-run `cargo test -p script-env --test follow` after any change — the thresholds there are set
from measurements, so a regression in density or dwell shows up immediately.

## `pulse.rhai`

What the light runs when the signal comes from audio rather than DMX, driven by
**crates/audio-bridge** reading a BlackHole clone of the deck's master output.

DMX carries no beat information, so `follow.rhai` has to reconstruct tempo from three colour
channels at ~5.7 usable frames/sec, and manages 0.364 on-grid concentration. With audio the
estimator moves to the Mac, where there is floating point and heap to spare, and this script
keeps only the part the light owns.

| | `follow.rhai` | `pulse.rhai` |
|---|---|---|
| on-grid @128 BPM | 0.364 | **0.871** |
| on-grid @100 BPM | — | **0.980** |
| relay ops/min R/Y/G | 216/90/240 | 223/169/272 |
| components | `array,math` | `array` |

**It is sent a prediction, not an event.** The block says "the next beat is in N ms, the period
is P", so the script runs the grid forward itself. That is what lets it schedule around the
100ms relay dwell instead of chasing a signal it is always a fraction of a beat behind, and it
makes loss cheap: 75% packet loss costs 0.871 → 0.833 rather than the lock.

### The beat block

Same magic, version and header as the DMX bridge — it is the same socket and the same parser in
`firmware/src/dmx.rs`. Two things differ.

`base` is `0xFFFE`. DMX bases are 1..512, so it cannot collide, and `follow.rhai` never reads
`base` — either script can tell the senders apart without being modified.

The payload sits in the **channel bytes**, read as `p.ch[0..16]`, not in an extended header.
The header is extensible and the firmware would skip what it does not recognise, but everything
before `header_len` is discarded before Rhai sees it: `dmx_recv` exposes only
`{ok, base, seq, ch}`. Beat fields in the header would mean reflashing, and the light needs
physical access.

| off | len | field | notes |
|---|---|---|---|
| 0 | 1 | `fmt` | `1` |
| 1 | 1 | `flags` | b0 audio, b1 tracking, b2 coasting, b3 bass muted, b4 bar valid, b5 clipping |
| 2 | 2 | `ms_to_next_beat` | BE, `0xFFFF` unknown |
| 4 | 2 | `period_ms` | BE, `0` unknown |
| 6 | 1 | `beat_index` | mod 16 |
| 7 | 1 | `confidence` | |
| 8–11 | 4 | `energy`, `low`, `mid`, `high` | AGC-normalised |
| 12 | 1 | `flux` | |
| 13 | 1 | `onset_age` | ms/4, `255` none |
| 14 | 1 | `onset_strength` | |
| 15 | 1 | `build` | `128` flat |

Milliseconds rather than a phase fraction, so `millis() + ms_to_next_beat` works without knowing
the period, and stays meaningful before one exists.

**`bar_valid` is never set.** `beat_index` counts from an arbitrary origin; there is no downbeat
estimator, so nothing may read `beat_index % 4 == 0` as a bar position until that bit appears.

The layout is written in `crates/audio-bridge/src/wire.rs` and read in `pulse.rhai`, with a third
copy in the test fixture. It is duplicated because the daemon has its own build root — aubio's
bindgen needs the macOS libclang, which the firmware's espup toolchain overrides globally.

### Two corrections worth keeping

- An accent that ended after the relay dwell put an off-grid transition on *every* beat, because
  100ms is not a slot boundary on a 117ms grid. Ending it on the next quarter took concentration
  from 0.812 to 0.871.
- The pattern cycle is forced to at least `4 × dwell`. At 175 BPM a quarter beat is 86ms and
  simply cannot be rendered, so the cycle doubles rather than asking for transitions the relays
  will refuse.

Re-run `cargo test -p script-env --test pulse` after any change.

## `dmxcap.mjs` and `watch.mjs`

The two ends of the DMX path, recorded so they can be compared. Run both through a set, alongside
`run-script.sh`:

```sh
node scripts/dmxcap.mjs dmx.txt      # what the light was told  (udp/49500)
node scripts/watch.mjs  light.jsonl  # what the light did       (the live socket)
```

`dmxcap.mjs` writes one frame per line, `arrival_ms seq base v0,v1,...`, which
`FOLLOW_CAPTURE` replays. Its Ctrl-C summary separates the three ways the stream can be wrong:
nothing arriving at all, frames arriving with every channel zero (the lighting engine running but
outputting blackout — nothing playing, or a venue mismatch), and real DMX. Because `seq` counts
what the bridge *sent*, the summary also prices the loss the receiver had to absorb.

`watch.mjs` records the public snapshot, which carries `lights`, `running` and per-lamp relay
`ops`. **`ops` counts since boot and the light is shared**, so a total is meaningless on its own —
the summary cuts a segment whenever `running` changes and attributes ops, lamp changes and time
to each job, so a script that never moved a given lamp is distinguishable from one that never ran.

## `logcat.mjs`

Collector for **dmx-bridge**'s UDP log sink — the only way to see anything from a bridge whose
console shares the USB PHY that TinyUSB has claimed.

```sh
node scripts/logcat.mjs 49510
```
