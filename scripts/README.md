# scripts

Helpers that aren't part of a build.

## `run-follow.sh`

Runs a script on the light for as long as you leave it running. Use this rather than a one-off
`curl`:

```sh
scripts/run-follow.sh                      # runs follow.rhai until Ctrl-C
scripts/run-follow.sh scripts/other.rhai
LOCK_S=120 scripts/run-follow.sh           # shorter cycle
```

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

`run-follow.sh` is the easy way to run it; by hand it is:

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
- **The output is dark 72% of the time, and the light must not be.** So DMX supplies only
  *colour* and *tempo*; the pattern itself is generated. Density is no longer bounded by
  rekordbox's duty cycle, which is what made every earlier version look sparse.
- **rekordbox sends one saturated colour at a time.** Measured off the wire: with a 3-channel
  RGB par patched at 1-3, one channel carries 255 and the other two sit at exactly zero, the
  live one rotating every few bars. A colour-faithful mapping onto three *coloured* lamps
  therefore lights one lamp at a time, which is sparse by construction. So the palette has a
  floor of **two** lamps — a frame that would light fewer lights all three, and the look is then
  restricted to the walking ones (chase, build, scatter) rather than the unison ones, since
  three lamps in unison is the fault signal.

Two AGC references, not one, because they answer different questions. Deciding *which lamp*
wants a **per-channel** peak, so a channel the venue only drives to half still uses its lamp.
Measuring *energy* wants a **single shared** peak across channels: under per-channel
normalisation a dim channel scales to ~1.0, so the max sits near 1.0 whenever anything is lit
and the envelope disappears. Conflating the two cost half the tempo lock (0.469 → 0.229).

### How it works

Per frame: per-channel AGC → normalised levels → a latched palette (which lamps this colour
lights, held through dark passages) → energy → rectified flux against an adaptive floor →
onsets. Onset intervals are octave-folded into 280-1000ms and fed to an agreement-gated EMA,
giving a beat period and a phase anchor. Three disagreeing intervals in a row are read as a
track change and taken as the new tempo.

The pattern then renders on a quarter-beat grid inside the palette, re-rolling `look` every
`LOOK_BEATS` beats. Each onset punches the whole palette through for one dwell period as an
accent, over whatever look is running.

| `look` | name | what it does |
|---|---|---|
| 0 | pump | palette on the front half of every cycle; breathes |
| 1 | chase | one palette lamp per half cycle, walking |
| 2 | stab | two hits per cycle with gaps, so the off is as loud as the on |
| 3 | offbeat | skips the downbeat entirely; syncopated against the track |
| 4 | build | one lamp when quiet, whole palette flickering at quarter resolution when loud |
| 5 | scatter | one or two palette lamps, re-rolled every quarter |

The chase, build and scatter looks walk a slot count of **at least two**, where a slot past the
end of the palette means all off. Without that, a colour lighting only one lamp made three of
the six looks completely static — measured as 6 transitions across a 6s green-only passage.

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

Tempo lock is measured against a control — circular concentration of transition times on the
quarter-beat grid of the tempo playing, versus the grid of a tempo that is not:

| stream | on its own grid | on the wrong grid | noise floor |
|---|---|---|---|
| 128 BPM | 0.469 | 0.010 | 0.051 |
| 100 BPM | 0.547 | 0.041 | 0.055 |

Density, across three streams — the third being the measured shape of real output, at the ~10Hz
the bridge actually delivers rather than the 41Hz DMX rate:

| stream | transitions/s | longest still gap |
|---|---|---|
| kick on every beat, 41Hz | 6.8 | — |
| sparse, mostly dark, 41Hz | 6.2 | 480ms |
| one saturated colour at a time, 10Hz | 6.6 | 580ms |

**This is the relay ceiling, not a design choice.** At that density the pattern costs 296 / 272 /
269 operations per minute for red / yellow / green against a datasheet maximum of 300/min. There
is no headroom left for a denser pattern on mechanical relays.

### Tuning

All of them are `let` bindings at the top of the file, in this order:

| name | effect |
|---|---|
| `LAMP_CUT` | normalised level at which a lamp joins the palette; lower lights more lamps together |
| `LOOK_BEATS` | beats before a new look is chosen; lower changes character more often |
| `THR_MULT`, `THR_BIAS` | onset sensitivity against the adaptive floor; lower finds more onsets and more accents |
| `PEAK_DECAY` | how fast the AGC ceiling falls, per step; lower adapts quicker to a dimmer track |
| `PEAK_FLOOR` | raw floor under the AGC ceiling, so a dark passage cannot amplify noise |
| `DARK` | normalised level below which a frame carries no colour and the palette latches |
| `STEP` | decision cadence in ms; kept above the ~41Hz DMX rate |
| `FLUX_SMOOTH`, `THR_SMOOTH` | EMA rates for the flux signal and its floor |
| `REFRACT` | minimum ms between onsets |
| `BEAT_MIN`, `BEAT_MAX` | the octave-folding window, 214 to 60 BPM |
| `BEAT_GAP` | an onset interval longer than this says nothing about tempo |
| `AGREE_PCT` | how far an interval may sit from the estimate and still be folded in |
| `DEAD_MS` | DMX silence before the bridge is assumed gone |

Re-run `cargo test -p script-env --test follow` after any change — the thresholds there are set
from measurements, so a regression in density or dwell shows up immediately.

## `logcat.mjs`

Collector for **dmx-bridge**'s UDP log sink — the only way to see anything from a bridge whose
console shares the USB PHY that TinyUSB has claimed.

```sh
node scripts/logcat.mjs 49510
```
