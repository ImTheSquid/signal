# scripts

Helpers that aren't part of a build.

## `run-script.sh`

Runs a script on the light for as long as you leave it running. Use this rather than a one-off
`curl`:

```sh
scripts/run-script.sh                      # runs pulse.rhai until Ctrl-C
scripts/run-script.sh scripts/other.rhai
LOCK_S=120 scripts/run-script.sh           # shorter cycle
COMPONENTS=array,math scripts/run-script.sh scripts/other.rhai
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

## `pulse.rhai`

The job the light runs, driven by **crates/audio-bridge** reading the master output — a
BlackHole clone of it on macOS, or a Scarlett capturing it on the Steam Deck
(`crates/audio-bridge/deck/`).

The tempo estimator lives on the host, which has floating point and heap to spare, and this
script keeps only the part the light owns.

| | `pulse.rhai` |
|---|---|
| on-grid @128 BPM | **0.951** |
| on-grid @100 BPM | **0.980** |
| relay ops/min R/Y/G | 168/202/165 |
| components | `array` |

**It is sent a prediction, not an event.** The block says "the next beat is in N ms, the period
is P", so the script runs the grid forward itself. That is what lets it schedule around the
100ms relay dwell instead of chasing a signal it is always a fraction of a beat behind, and it
makes loss cheap: 75% packet loss costs 0.951 → 0.949 rather than the lock.

### Where the period comes from

A neural one. TempoCNN classifies the tempo over 256 absolute bins from a mel spectrogram, and
`AubioTracker` keeps supplying only the *phase* — where the beats fall. Scored against
rekordbox's own beat grids for 44 library tracks, extracted by `crates/grid-truth`:

| tempo source | correct |
|---|---|
| aubio as it was | 32% |
| confidence thresholds calibrated against music rather than a click train | 52% |
| a comb over onset novelty, resolving the metrical level | 68% |
| **TempoCNN** | **84%** |

The whole gap is metrical level, not precision. aubio reports *a* periodicity and often the
wrong multiple of it — five of its wrong tempi were exactly 2/3 of the truth and two exactly
half. A comb score cannot fix that in general because it has no notion of which tempi are
plausible, so it cannot tell a correct 127 BPM from a 127 that should be 190. A classifier can,
because 87 and 174 are separate classes competing on evidence.

None of this changed the script. `pulse.rhai` reads `period_ms` and runs the grid forward, so a
period that is right 84% of the time instead of 68% improves the look for free — a good pattern
on a wrong tempo is only wrong faster.

### What decides what is lit

Not a fixed set of looks. An earlier six-look version measured **1.80x** on the one test that
matches what an observer does — train a lookup table on half a run, predict the other half —
against a floor of guessing the commonest state forever. Nothing repeated verbatim (0.000 at
four beats and at eight) and it was still readable, because six deterministic programs behind a
slow mode switch is a grammar: three or four slots identify which look is running and the look
determines the rest. Marginal statistics could not see that.
`an_observer_learns_nothing_by_watching` can, and the parametric generator below measures
**1.01x**.

Every cycle is decided at its first slot and then only read back. Drawing per slot instead
re-rolls what the lamps have already answered.

| drawn | every | what it is |
|---|---|---|
| `perm` | 5 cycles | which lamp leads, drawn weighted by band level rather than ranked by it |
| `hold_lamp` | 7 cycles | one lamp rides the whole cycle, a third of the time, never twice running |
| `onsets`, `rot` | 1–3 cycles | the rhythm: E(k,4) rotated — a Euclidean pattern and one of its rotations |
| `ph`, `walk`, width | every cycle | where in `perm` each attack starts, and how far it steps |

The periods are pairwise coprime and coprime with the four-slot grid, so the clocks never reset
together — joint period 35 cycles rather than 2. Powers of two nested on a shared origin was the
real fault: the *values* were random, the *change points* were not, and change points are what
the eye locks onto.

Three things are deliberate and were each arrived at by measuring the alternative:

- **Weighted, not ranked.** Ranking the lamps by band level is a colour organ, and popular music's
  spectrum is stable for bars at a time, so bass meant red for a whole section. Drawing the leader
  with the bands as weights keeps the music visible without being a function of it. Equal bands
  fall out as a uniform draw, so a dead-colour stretch needs no special case.
- **The rhythm rides, the lamps do not.** Re-drawing everything every cycle took beat-to-beat
  repetition to 0.000, which reads as flicker rather than phrasing. A random 1–3 cycle span, so
  the boundary is not itself learnable.
- **Width includes zero.** A non-attack slot holds what the last attack left, so drawing width
  high pins the light near fully lit — measured at 0.77 duty and 2.34 bits/slot, brighter and
  blander than the 0.63 and 2.71 it gets by allowing dark attacks.

### The beat block

It rides the light's existing UDP path — the same socket and the same parser in
`firmware/src/dmx.rs`, which is why no reflash was needed to add it. Two things are worth
knowing.

`base` is `0xFFFE`, outside the 1..512 a channel-oriented sender can use, so a script can
tell senders apart without being modified.

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

**`bar_valid` is never set**, so `beat_index` still counts from an arbitrary origin and nothing
may read `beat_index % 4 == 0` as a bar position.

Not for want of an estimator — `crates/audio-bridge/src/downbeat.rs` is one, and it finds *a*
stable 16-beat phase far above chance. It cannot reliably find *which* phase is the downbeat: it
locks to the largest recurring novelty, which is sometimes the beat-one crash and sometimes a
fill on fifteen or a drop on nine. Measured on 37 tracks whose tempo is correct, phrase
consistency is 53% and bar 64%.

Five things were tried and none moved it: the much better tempo grid (50% → 53%, so it was never
an alignment artefact), dropping the kick band on the theory that a beat-every-kick dilutes the
bins (worse, 53% → 48% — kick patterns are not uniform, they drop out in breakdowns and double on
fills), anchor hysteresis (identical, so the anchor is not flipping), and tempo drift as a
confound (lowest-drift third 62%, highest 59%).

Gating on confidence does not rescue it either. Swept over 44 tracks: ungated 51% of beats
correct on 98% of beats, at 4 sigma 59% on 18%, at 8 sigma 54% on 2%, at 12 sigma 33%. Past 4
sigma the confidence *anti*-predicts, so there is no threshold that selects the cases worth
acting on. Four wrong beats in ten reads as a mistake rather than as looseness, which is why the
flag stays clear and the script keeps no phrase input.

The route past this is a learned downbeat tracker, where online state of the art is about 53%
F1 — roughly where this already sits.

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

### Cadence is now a secondary guard

The modal-gap-share bar was 0.52 and is 0.55. It was a proxy for predictability adopted when
there was no direct measure, and the two pull against each other: a sparser rhythm spreads the
gaps *and* makes the next slot easier to guess, because holding still is predictable. Measured,
biasing toward sparse moved cadence 0.51 → 0.49 and predictability 1.01x → 1.17x. The concern
cadence encoded — that the eye gives up on *which* lamp and predicts *when* — is now tested
directly by conditioning the observer on the position in the beat, which buys it nothing.

### The RNG has to be real

Everything here rests on `rand_int`, so where its entropy comes from is load-bearing rather than
incidental. `Handlers::stubs()` seeds a fixed xorshift on purpose, so validation is reproducible;
the tests override it per seed, because one draw of a stochastic script measures one draw. The
device passes `esp_random()` — the hardware RNG, true random with RF up, which it always is by
the time a packet has arrived. A fixed seed anywhere on that path would mean the light replays
one identical pattern from every boot, and none of the rest of this would reach the room.

Re-run `cargo test -p script-env --test pulse` after any change. `tests/entropy_probe.rs` prints
the same measures across nine seeds and ten simulated minutes; it asserts nothing, and it is
where a tuning decision should be checked before it becomes a bar in `pulse.rs`.

## `watch.mjs`

What the light actually did, recorded from the public live socket. Run it through a set
alongside `run-script.sh`:

```sh
node scripts/watch.mjs light.jsonl
```

It records the public snapshot, which carries `lights`, `running` and per-lamp relay `ops`.
**`ops` counts since boot and the light is shared**, so a total is meaningless on its own — the
summary cuts a segment whenever `running` changes and attributes ops, lamp changes and time to
each job, so a script that never moved a given lamp is distinguishable from one that never ran.
