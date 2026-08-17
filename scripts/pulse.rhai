fn pickn(ord, start, k) {
    let out = [false, false, false];
    let i = 0;
    while i < k && i < 3 {
        out[ord[(start + i) % 3]] = true;
        i += 1;
    }
    out
}

fn enc(s) {
    let c = 0;
    if s[0] { c += 1; }
    if s[1] { c += 2; }
    if s[2] { c += 4; }
    c
}

fn bit(c, i) {
    let d = c;
    if i == 1 { d = c / 2; }
    if i == 2 { d = c / 4; }
    d % 2 == 1
}

// E(k,4) — k attacks spread as evenly as four slots allow, which is the
// Euclidean rhythm and most of the world's traditional ones. Three coin flips
// gave a binomial instead, so it clumped and then held still.
fn hits(i, k) {
    ((i * k) % 4) < k
}

// The lamp order, drawn rather than ranked. Weighting by band level keeps the
// music visible in which lamp leads; drawing it stops the eye learning that bass
// means red, which is a colour organ and is what ranking made this. Equal bands
// fall out as a uniform draw, so a dead colour stretch needs no special case.
fn wperm(a, b, c) {
    let tot = a + b + c + 3;
    let r = rand_int(0, tot - 1);
    let lead = 2;
    if r < a + 1 {
        lead = 0;
    } else if r < a + b + 2 {
        lead = 1;
    }
    let x = (lead + 1) % 3;
    let y = (lead + 2) % 3;
    if rand_int(0, 1) == 1 {
        let t = x;
        x = y;
        y = t;
    }
    [lead, x, y]
}

let STEP = 20;
let AUDIO_BASE = 65534;
let FMT_BEAT = 1;
let BLOCK_LEN = 16;
let UNKNOWN = 65535;
let DEAD_MS = 8000;
let PATTERN_MAX = 700;
let WANDER_MS = 700;
let WIDE_AT = 70;
let FULL_AT = 115;

// Pairwise coprime, and coprime with the four-slot grid, so the clocks that
// refresh the pattern's random state never reset together. Powers of two nested
// on a shared origin meant every clock restarted every eight beats: the values
// were random but the change points were not, and change points are what the eye
// locks onto. Joint period is now 35 cycles rather than 2.
let PERM_CYCLES = 5;
let HOLD_CYCLES = 7;

let DWELL = lamp_dwell_ms();
if DWELL < 1 { DWELL = 1; }

let period = WANDER_MS;
let beat_count = 0;
let lvl = 0;
let build = 128;
let low = 0;
let mid = 0;
let high = 0;
let bass_muted = false;

let perm = [0, 1, 2];
let perm_until = rand_int(0, PERM_CYCLES - 1);
let hold_lamp = -1;
let hold_until = rand_int(0, HOLD_CYCLES - 1);
let cyc_count = 0;
let onsets = 1;
let rot = 0;
let rhy_until = 0;
let plan = [0, 0, 0, 0];
let planned = false;
let last_slot = -1;
let accent_until = -99999;

let cur_r = false;
let cur_y = false;
let cur_g = false;
let last_change = -99999;
let nudge = false;
let nudge_new = false;
let nudge_s = [false, false, false];

let t0 = millis();
let next = t0;
let beat_at = t0 + period;
let last_pkt = t0 - DEAD_MS;

loop {
    let p = dmx_recv(STEP);
    let now = millis();

    if p.ok && p.base == AUDIO_BASE && p.ch.len() >= BLOCK_LEN && p.ch[0] == FMT_BEAT {
        last_pkt = now;

        let flags = p.ch[1];
        let to_next = p.ch[2] * 256 + p.ch[3];
        let pd_ms = p.ch[4] * 256 + p.ch[5];
        let energy = p.ch[8];
        low = p.ch[9];
        mid = p.ch[10];
        high = p.ch[11];
        build = p.ch[15];
        bass_muted = (flags / 8) % 2 == 1;

        if pd_ms > 0 && to_next != UNKNOWN {
            period = pd_ms;
            beat_at = now + to_next;
        }

        lvl += (energy - lvl) / 6;
    }

    let live = now - last_pkt < DEAD_MS;

    if period < 1 { period = WANDER_MS; }

    // Never subdivide below the relay dwell: at 175 BPM a quarter beat is 86ms
    // and simply cannot be rendered, so the pattern runs on a longer cycle.
    let pd = period;
    while pd > PATTERN_MAX { pd /= 2; }
    while pd < DWELL * 4 { pd *= 2; }

    // Run the grid forward locally. A dropped packet costs nothing: the period
    // is known, so the next beat follows from the last one without being told.
    //
    // An accent runs to the next quarter rather than to the dwell, so the lamps
    // come back on a slot boundary. Ending it at an arbitrary offset put one
    // off-grid transition on every beat.
    while now >= beat_at {
        beat_at += period;
        beat_count += 1;
        if live && lvl > FULL_AT { accent_until = now + pd / 4; }
    }

    // Position on the pattern cycle, in thousandths. Measured in beats rather
    // than milliseconds so a cycle spanning two beats keeps a stable origin
    // instead of restarting on every one.
    let to_next = beat_at - now;
    let bpos = beat_count * 1000 - to_next * 1000 / period;
    let cycle = pd * 1000 / period;
    if cycle < 1 { cycle = 1; }
    let cyc = bpos % cycle;
    if cyc < 0 { cyc += cycle; }
    let frac = cyc * 1000 / cycle;
    let quarter = frac / 250;

    // How wide the pattern wants to be. `build` is the daemon's slow trend, so
    // density drifts with the arrangement instead of stepping on a threshold —
    // a plateau that holds for a whole section is a plateau the eye can price in.
    let bias = (build - 128) / 8;
    let w = 1;
    if lvl + bias > WIDE_AT { w = 2; }
    if lvl + bias > FULL_AT { w = 3; }
    if live && w < 2 { w = 2; }
    // A bass kill is the tension before a drop. Going sparse here is what makes
    // the widening afterwards read as a release.
    if bass_muted { w = 1; }

    if quarter != last_slot {
        // Arm the movement guarantee only here, at a slot boundary, so what it
        // forces lands on the grid like every other change. Armed the moment the
        // stillness threshold passes, it fires at an arbitrary offset instead.
        nudge = now - last_change >= pd;
        nudge_new = nudge;
        last_slot = quarter;

        // The whole cycle is decided at its first slot, once, and then only read
        // back. Drawing per slot instead re-rolls what the lamps already answered
        // and lands a second change at an arbitrary offset inside the slot.
        if quarter == 0 || !planned {
            planned = true;
            cyc_count += 1;

            if cyc_count >= perm_until {
                perm_until = cyc_count + PERM_CYCLES;
                perm = wperm(low, mid, high);
            }

            // One lamp rides through a whole cycle, some cycles. Uniform business
            // is most of what makes a pattern feel mechanical, and a lamp sitting
            // still gives the others something to move against. It is also the
            // cheapest thing here in relay wear rather than the most expensive.
            if cyc_count >= hold_until {
                hold_until = cyc_count + HOLD_CYCLES;
                let was = hold_lamp;
                hold_lamp = -1;
                // Never the same lamp twice running: two consecutive picks chained
                // into a lamp sitting still for twenty beats, which reads as broken
                // rather than as contrast.
                if rand_int(0, 2) == 0 {
                    hold_lamp = perm[rand_int(0, 2)];
                    if hold_lamp == was { hold_lamp = -1; }
                }
            }

            // The rhythm rides for one to three cycles; the lamps it is played on
            // never do. This is the point of the rewrite — six fixed looks behind
            // a slow mode switch is a grammar, and a few slots identify which look
            // is running. But re-drawing everything every cycle is the other
            // failure: it left nothing for two consecutive beats to have in common,
            // which reads as flicker rather than as phrasing. A random span, so the
            // boundary the rhythm changes on is not itself something to learn.
            if cyc_count >= rhy_until {
                rhy_until = cyc_count + rand_int(1, 3);
                // How busy the cycle is, scaled by energy but not pinned to it.
                // Tying this to the width directly made a loud passage play E(3,4)
                // every cycle, which is a metronome wearing a Euclidean hat.
                onsets = rand_int(1, w);
                // Rotation is free variety — every rotation of a Euclidean pattern
                // is another one. Biased to keep the downbeat, so the beat lands.
                rot = rand_int(0, 3);
                if rand_int(0, 1) == 1 { rot = 0; }
            }

            let ph = rand_int(0, 2);
            let walk = rand_int(1, 2);

            let cur = enc([cur_r, cur_y, cur_g]);
            let i = 0;
            while i < 4 {
                if hits((i + rot) % 4, onsets) {
                    // Width per attack, dark included. A non-attack slot holds what
                    // the last attack left, so drawing width high keeps the light
                    // pinned near fully lit: bright, and blander than it measures.
                    let kk = rand_int(0, w);

                    let want = enc(pickn(perm, ph, kk));
                    // An attack that renders what is already lit is a silent beat.
                    // Changing the count always reads as movement, and narrowing by
                    // one is total — from dark it widens instead.
                    if want == cur {
                        if kk > 0 { kk -= 1; } else { kk = 1; }
                        want = enc(pickn(perm, ph, kk));
                    }
                    cur = want;
                    ph = (ph + walk) % 3;
                }
                plan[i] = cur;
                i += 1;
            }

            if hold_lamp >= 0 {
                let m = 1;
                if hold_lamp == 1 { m = 2; }
                if hold_lamp == 2 { m = 4; }
                i = 0;
                while i < 4 {
                    if (plan[i] / m) % 2 == 0 { plan[i] += m; }
                    i += 1;
                }
            }
        }
    }

    let code = plan[quarter];
    let s = [bit(code, 0), bit(code, 1), bit(code, 2)];

    if !live {
        // Deliberately neither a red-yellow-green sequence nor all three at 1Hz:
        // the first reads as an ordinary traffic light, the second is the
        // firmware's fault signal.
        s = pickn(perm, cyc_count, 2);
    } else if now < accent_until {
        s = [true, true, true];
    }

    // Backstop only. The rhythm guarantees an attack every cycle and an attack
    // always changes something, so this now catches the accent and dead-stream
    // paths rather than the pattern itself.
    if nudge {
        if nudge_new {
            nudge_new = false;
            if cur_r || cur_y || cur_g {
                nudge_s = [false, false, false];
                if hold_lamp >= 0 { nudge_s[hold_lamp] = true; }
            } else {
                let k = w;
                if k < 1 { k = 1; }
                nudge_s = pickn(perm, cyc_count, k);
            }
        }
        s = nudge_s;
    }

    // Gate on the dwell rather than letting set_lights block: a blocked call
    // stalls the loop, and a stalled loop misses packets.
    if (s[0] != cur_r || s[1] != cur_y || s[2] != cur_g) && now - last_change >= DWELL {
        set_lights(s[0], s[1], s[2]);
        cur_r = s[0]; cur_y = s[1]; cur_g = s[2];
        last_change = now;
    }

    next += STEP;
    if next < now { next = now; }
    sleep_until(next);
}
