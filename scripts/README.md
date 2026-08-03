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
firmware change, so tune it freely. `run-follow.sh` is the easy way to run it; by hand it is:

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

Peaks were `R=255 G=131 B=255`, median non-zero level 118. Two consequences drove the design:

- A fixed `>= 128` cut discards nearly half the output, and since green peaks at 131 the green
  lamp would almost never fire. So lamps are chosen **relative to the brightest channel**.
- rekordbox drives the fixture in short accents, so instantaneous mapping reads as flicker.
  Each hit is **latched for `HOLD_MS`**, deliberately above the 100ms relay dwell so the hold
  survives the throttle instead of being coalesced away.

**28% is a ceiling set by rekordbox, not by this script.** If it still feels sparse, change the
fixture's macro role (the non-Simple `Par Light 1`, or `Bar Light 1`) or author a denser pattern
in MACRO EDITOR. No script change will exceed what the macro engine emits.

### Tunables

| name | effect |
|---|---|
| `FLOOR` | below this the fixture counts as dark; lower catches fainter output |
| `SHARE` | a lamp lights at >= `SHARE`/4 of the brightest channel; lower means more lamps on together |
| `HOLD_MS` | how long one hit holds a lamp; longer is denser but blurs fast hits |
| `QUIET_MS` | DMX silence before the fallback sweep takes over, so the light is never dead |

## `logcat.mjs`

Collector for **dmx-bridge**'s UDP log sink — the only way to see anything from a bridge whose
console shares the USB PHY that TinyUSB has claimed.

```sh
node scripts/logcat.mjs 49510
```
