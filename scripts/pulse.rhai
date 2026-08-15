fn pickn(ord, start, k) {
    let out = [false, false, false];
    let i = 0;
    while i < k && i < 3 {
        out[ord[(start + i) % 3]] = true;
        i += 1;
    }
    out
}

fn order3(a, b, c) {
    let ord = [0, 1, 2];
    let lev = [a, b, c];
    if lev[ord[1]] > lev[ord[0]] { let t = ord[0]; ord[0] = ord[1]; ord[1] = t; }
    if lev[ord[2]] > lev[ord[1]] { let t = ord[1]; ord[1] = ord[2]; ord[2] = t; }
    if lev[ord[1]] > lev[ord[0]] { let t = ord[0]; ord[0] = ord[1]; ord[1] = t; }
    ord
}

let STEP = 20;
let AUDIO_BASE = 65534;
let FMT_BEAT = 1;
let BLOCK_LEN = 16;
let UNKNOWN = 65535;
let DEAD_MS = 8000;
let LOOK_BEATS = 8;
let GROUP = 4;
let PATTERN_MAX = 700;
let WANDER_MS = 700;
let COLOUR_DEAD = 77;
let WIDE_AT = 70;
let FULL_AT = 115;

let DWELL = lamp_dwell_ms();
if DWELL < 1 { DWELL = 1; }

let period = WANDER_MS;
let beat_count = 0;
let lvl = 0;
let ord = [0, 1, 2];
let bass_muted = false;
let was_muted = false;

let look = 0;
let look_until = 0;
let hold_lamp = -1;
let hold_until = 0;
let mask = [true, true, true, true];
let last_slot = -1;
let scatter_start = 0;
let scatter_w = 1;
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
        let low = p.ch[9];
        let mid = p.ch[10];
        let high = p.ch[11];
        bass_muted = (flags / 8) % 2 == 1;

        if pd_ms > 0 && to_next != UNKNOWN {
            period = pd_ms;
            beat_at = now + to_next;
        }

        let cmax = low;
        if mid > cmax { cmax = mid; }
        if high > cmax { cmax = high; }
        if cmax < COLOUR_DEAD {
            let s0 = beat_count % 3;
            ord = [s0, (s0 + 1) % 3, (s0 + 2) % 3];
        } else {
            ord = order3(low, mid, high);
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
    let half = frac / 500;

    let w = 1;
    if lvl > WIDE_AT { w = 2; }
    if lvl > FULL_AT { w = 3; }
    if live && w < 2 { w = 2; }
    // A bass kill is the tension before a drop. Going sparse here is what makes
    // the widening afterwards read as a release.
    if bass_muted { w = 1; }
    let rest = w - 1;
    if rest < 1 { rest = 1; }

    if quarter != last_slot {
        // Arm the movement guarantee only here, at a slot boundary, so what it
        // forces lands on the grid like every other change. Armed the moment the
        // stillness threshold passes, it fires at an arbitrary offset instead —
        // which is what a masked group full of deliberate stillness turns into.
        nudge = now - last_change >= pd / 2;
        nudge_new = nudge;
        if quarter % 2 == 0 {
            scatter_start = rand_int(0, 2);
            scatter_w = rand_int(1, w);
        }
        last_slot = quarter;
    }

    if beat_count >= look_until || bass_muted != was_muted {
        was_muted = bass_muted;
        look_until = beat_count + LOOK_BEATS;
        look = rand_int(0, 5);
    }

    // A four-beat sentence. At 128 BPM the pattern cycle is exactly one beat, so
    // every look repeats itself once a beat and the eye finishes it after two.
    // Ending the group differently is what makes it read as phrasing.
    let group = beat_count % GROUP;

    // One lamp rides through a whole group, some groups. Uniform business is
    // most of what makes a pattern feel mechanical, and a lamp that sits still
    // while the others move gives the rest something to move against. It is also
    // the cheapest thing here in relay wear rather than the most expensive.
    if beat_count >= hold_until {
        hold_until = beat_count + GROUP;
        hold_lamp = -1;
        if rand_int(0, 2) == 0 { hold_lamp = ord[rand_int(0, 2)]; }

        // Which quarters this group may change on. The first always, so the beat
        // itself still lands; the rest by chance.
        //
        // This is what stops the cadence giving the look away. Changing something
        // at every quarter of every beat is metronomic whatever the lamps do —
        // the eye gives up predicting which and starts predicting when, and it is
        // never wrong. Skipping slots is syncopation and space, and it costs
        // relay operations rather than spending them.
        mask = [true, rand_int(0, 1) == 1, rand_int(0, 1) == 1, rand_int(0, 1) == 1];
    }

    // The last beat of the group plays the same rhythm on different lamps, so
    // the sentence has an ending rather than four identical words.
    //
    // Rotating rather than scattering on purpose: a busier fill costs extra
    // transitions inside the beat, and at 117ms quarters against a 100ms dwell
    // those arrive late and land off the grid. Same count, different lamps, and
    // the grid survives.
    let play = ord;
    if group == GROUP - 1 { play = [ord[1], ord[2], ord[0]]; }

    let s = [false, false, false];

    if !live {
        s = pickn(play,beat_count, 2);
    } else if now < accent_until {
        s = pickn(play,0, 3);
    } else if look ==0 {
        if quarter < 2 { s = pickn(play,0, w); } else { s = pickn(play,0, rest); }
    } else if look ==1 {
        s = pickn(play,beat_count * 2 + half, 1);
    } else if look ==2 {
        if quarter == 0 || quarter == 2 { s = pickn(play,0, w); } else { s = pickn(play,half, rest); }
    } else if look ==3 {
        if quarter >= 2 { s = pickn(play,half, w); } else { s = pickn(play,beat_count, rest); }
    } else if look ==4 {
        let k = quarter + 1;
        if k > 3 { k = 1; }
        if k > w + 1 { k = w + 1; }
        s = pickn(play,0, k);
    } else {
        s = pickn(play,scatter_start, scatter_w);
    }

    // The held lamp overrides the look, but never the accent or the dead-stream
    // pattern: one is the loudest moment in the track and the other is the only
    // sign the daemon is gone.
    if live && now >= accent_until && hold_lamp >= 0 { s[hold_lamp] = true; }

    // Hold through the quarters this group is not allowed to change on. Last,
    // so it holds what is actually lit rather than what the look wanted.
    if live && now >= accent_until && !mask[quarter] { s = [cur_r, cur_y, cur_g]; }

    // Guarantee something moves every half cycle, so a steady passage cannot
    // leave the light frozen. Armed at the slot boundary above; this is what it
    // does when it fires.
    if nudge {
        // Once per slot, not once per loop: recomputing it after the lamps have
        // already answered gives a different answer, and firing that lands a
        // second change at an arbitrary offset inside the slot.
        if nudge_new {
            nudge_new = false;
            if cur_r || cur_y || cur_g {
                nudge_s = [false, false, false];
                if hold_lamp >= 0 { nudge_s[hold_lamp] = true; }
            } else {
                let k = w;
                if k < 1 { k = 1; }
                nudge_s = pickn(play,beat_count, k);
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
