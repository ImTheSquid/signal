// Follow rekordbox lighting on the traffic light.
//
// Three lamps, on/off only, and a relay dwell floor — so this cannot be a
// dimmer. What it can be is *rhythmic*: something changes on every subdivision
// of the beat, in the colour the music is in. Density is the whole point. An
// occasional accent over a mostly-static light reads as a broken traffic signal.
//
// Four measurements from a real set shape the analysis:
//
// 1. rekordbox's output is dark ~72% of the time, so dark frames must not be
//    treated as "no data" — the last real colour is latched and kept.
//
// 2. Channels do not share a range. A measured set peaked R=255 G=131 B=255;
//    any cut taken against the frame maximum needed 204 and so could never
//    admit green at all. Hence a *per-channel* running peak: each channel is
//    normalised by its own ceiling, so a channel the venue only ever drives to
//    half still gets full use of its lamp.
//
// 3. Absolute levels are useless ([3,4,6] is a typical frame). Everything below
//    works on normalised levels and on *change*, not on brightness.
//
// 4. rekordbox's lighting engine is itself beat-synced, so the onsets in this
//    stream land on beats. Estimating the period lets the pattern subdivide and
//    predict, instead of only reacting — which is what makes it dense.
//
// Deliberately NOT used: all three lamps steady at 1Hz. That is the firmware's
// fault signal, and it has to stay unambiguous. Brief all-three flashes are fine
// and are clearly something else.

// Walk position `i` of `n` over the palette. `n` is at least 2 and out-of-range
// means all off, so a colour that lights only one lamp pulses instead of
// standing still — measured as three of the six looks going completely static
// through a green-only passage.
fn pick(pal, i) {
    let out = [false, false, false];
    if i < pal.len() { out[pal[i]] = true; }
    out
}

// ---- tunables -------------------------------------------------------------

let STEP = 20;            // decision cadence in ms, above the ~41Hz DMX rate
let PEAK_DECAY = 0.999;   // per step, per-channel AGC ceiling: ~5%/s
let PEAK_FLOOR = 20.0;    // raw units; stops a dark passage amplifying noise
let LAMP_CUT = 0.30;      // normalised level at which a lamp joins the palette
let DARK = 0.12;          // below this the frame carries no colour information
let FLUX_SMOOTH = 0.30;
let THR_SMOOTH = 0.02;    // EMA of flux; the adaptive onset floor
let THR_MULT = 1.8;
let THR_BIAS = 0.03;
let REFRACT = 110;        // ms; no second onset inside this
let BEAT_MIN = 280;       // ms (214 BPM)
let BEAT_MAX = 1000;      // ms (60 BPM)
let BEAT_GAP = 3000;      // an interval longer than this says nothing about tempo
let AGREE_PCT = 22;       // an interval may differ from the estimate by this much
let LOOK_BEATS = 8;       // beats before a new look is chosen
let DEAD_MS = 8000;       // no DMX at all this long => the bridge itself is gone

// The hardware floor. Every transition below is gated on it, so set_lights is
// never asked to block — a blocked call would stall this loop and drop frames.
let DWELL = lamp_dwell_ms();
if DWELL < 1 { DWELL = 1; }

// ---- analysis state -----------------------------------------------------

let pk_r = PEAK_FLOOR;
let pk_g = PEAK_FLOOR;
let pk_b = PEAK_FLOOR;
let e_prev = 0.0;
let flux = 0.0;
let thr = 0.05;
let lvl = 0.0;            // smoothed energy, 0..1

let last_onset = -99999;
let period = 500;         // ms/beat, 120 BPM until measured otherwise
let anchor = 0;           // ms of the last beat we are confident about
let disagree = 0;

// Latched palette: which lamps the current colour lights. All three initially so
// there is something to show before the first frame.
let lr = true;
let ly = true;
let lg = true;

// Beat phase bookkeeping.
let beat_count = 0;
let last_slot = -1;
let scatter_a = 0;        // look 5 re-rolls these once per slot
let scatter_b = -1;

let look = 0;
let look_until = 0;
let accent_until = -99999;

// What is physically on, and when it last changed.
let cur_r = false;
let cur_y = false;
let cur_g = false;
let last_change = -99999;

let t0 = millis();
let next = t0;
let last_pkt = t0;
anchor = t0;

loop {
    let p = dmx_recv(STEP);
    let now = millis();

    if p.ok {
        last_pkt = now;

        // ---- per-channel AGC ---------------------------------------------
        let r = p.ch[0].to_float();
        let g = p.ch[1].to_float();
        let b = p.ch[2].to_float();

        pk_r *= PEAK_DECAY;
        pk_g *= PEAK_DECAY;
        pk_b *= PEAK_DECAY;
        if pk_r < PEAK_FLOOR { pk_r = PEAK_FLOOR; }
        if pk_g < PEAK_FLOOR { pk_g = PEAK_FLOOR; }
        if pk_b < PEAK_FLOOR { pk_b = PEAK_FLOOR; }
        if r > pk_r { pk_r = r; }
        if g > pk_g { pk_g = g; }
        if b > pk_b { pk_b = b; }

        let nr = r / pk_r;
        let ng = g / pk_g;
        let nb = b / pk_b;

        // ---- palette -----------------------------------------------------
        let mx = nr;
        if ng > mx { mx = ng; }
        if nb > mx { mx = nb; }
        if mx > DARK {
            // Patched as RGB, so blue drives the yellow lamp.
            lr = nr >= LAMP_CUT;
            lg = ng >= LAMP_CUT;
            ly = nb >= LAMP_CUT;
            // Never empty — the brightest channel always counts, or a colour
            // just under the cut would black the light out.
            if !lr && !ly && !lg {
                if nr >= ng && nr >= nb { lr = true; }
                else if ng >= nb { lg = true; }
                else { ly = true; }
            }
        }

        // ---- energy, rectified flux, onset --------------------------------
        let e = (nr + ng + nb) / 3.0;
        let d = e - e_prev;
        if d < 0.0 { d = 0.0; }   // only rises are onsets
        e_prev = e;
        flux += (d - flux) * FLUX_SMOOTH;
        lvl += (e - lvl) * 0.06;

        if flux > thr * THR_MULT + THR_BIAS && now - last_onset >= REFRACT {
            let iv = now - last_onset;
            last_onset = now;
            accent_until = now + DWELL;

            // ---- beat period -------------------------------------------
            if iv <= BEAT_GAP {
                // Octave-fold: onsets often land on halves or doubles of the
                // beat, and those carry the same tempo information.
                while iv < BEAT_MIN { iv *= 2; }
                while iv > BEAT_MAX { iv /= 2; }
                if iv >= BEAT_MIN {
                    let err = iv - period;
                    if err < 0 { err = -err; }
                    if err * 100 <= period * AGREE_PCT {
                        period = (period * 4 + iv) / 5;
                        anchor = now;      // phase 0 is the hit itself
                        disagree = 0;
                    } else {
                        // Two disagreeing intervals are noise; several in a row
                        // mean the track changed, so take the new tempo.
                        disagree += 1;
                        if disagree >= 3 {
                            period = iv;
                            anchor = now;
                            disagree = 0;
                        }
                    }
                }
            }
        }
        // The adaptive floor tracks flux, so a loud passage does not read as one
        // continuous onset and a quiet one still triggers.
        thr += (flux - thr) * THR_SMOOTH;
    }

    // ---- beat phase ------------------------------------------------------

    // The pattern cycle, which is the beat unless the beat is too fast to
    // render. Doubling rather than clamping keeps it musical: at 180 BPM the
    // pattern runs in half time instead of drifting against the track.
    let pd = period;
    while pd < DWELL * 4 { pd *= 2; }

    let since = now - anchor;
    if since < 0 { since = 0; }
    let frac = (since % pd) * 1000 / pd;   // thousandths through the cycle
    let quarter = frac / 250;              // 0..3

    // The palette as an indexable list, so a chase walks only the lamps the
    // colour actually lights instead of stalling on the ones it does not.
    let pal = [];
    if lr { pal.push(0); }
    if ly { pal.push(1); }
    if lg { pal.push(2); }
    if pal.len() == 0 { pal.push(0); }
    let n = pal.len();
    if n < 2 { n = 2; }

    if quarter != last_slot {
        if quarter == 0 { beat_count += 1; }
        last_slot = quarter;
        scatter_a = rand_int(0, n - 1);
        scatter_b = -1;
        if rand_chance(0.45) { scatter_b = rand_int(0, n - 1); }
    }

    if beat_count >= look_until {
        look_until = beat_count + LOOK_BEATS;
        look = rand_int(0, 5);
    }

    // ---- render ----------------------------------------------------------
    let wr = false;
    let wy = false;
    let wg = false;

    if now - last_pkt >= DEAD_MS {
        // Nothing arriving. Not a red-yellow-green sequence, and not the
        // firmware's steady 1Hz — a slow wander, so the light reads as waiting.
        let k = (beat_count / 2) % 3;
        wr = k == 0;
        wy = k == 1;
        wg = k == 2;
    } else if now < accent_until {
        // The hit. Whole palette, briefly, over whatever look is running.
        wr = lr; wy = ly; wg = lg;
    } else if look == 0 {
        // PUMP — palette on the front half of every cycle. Breathes.
        let on = frac < 500;
        wr = lr && on; wy = ly && on; wg = lg && on;
    } else if look == 1 {
        // CHASE — one palette lamp per half cycle, walking.
        let s = pick(pal, (beat_count * 2 + frac / 500) % n);
        wr = s[0]; wy = s[1]; wg = s[2];
    } else if look == 2 {
        // STAB — two hits per cycle with gaps, so the off is as loud as the on.
        let on = frac < 250 || (frac >= 500 && frac < 750);
        wr = lr && on; wy = ly && on; wg = lg && on;
    } else if look == 3 {
        // OFFBEAT — skips the downbeat entirely. Syncopated against the track.
        let on = frac >= 500 && frac < 750;
        wr = lr && on; wy = ly && on; wg = lg && on;
    } else if look == 4 {
        // BUILD — width follows energy: one lamp when quiet, the whole palette
        // flickering at quarter resolution when loud.
        if lvl > 0.55 {
            let on = quarter % 2 == 0;
            wr = lr && on; wy = ly && on; wg = lg && on;
        } else {
            let s = pick(pal, beat_count % n);
            wr = s[0]; wy = s[1]; wg = s[2];
        }
    } else {
        // SCATTER — one or two palette lamps, re-rolled every quarter.
        let s = pick(pal, scatter_a);
        wr = s[0]; wy = s[1]; wg = s[2];
        if scatter_b >= 0 {
            let s2 = pick(pal, scatter_b);
            wr = wr || s2[0]; wy = wy || s2[1]; wg = wg || s2[2];
        }
    }

    // Gate on the dwell here rather than letting set_lights block: a blocked
    // call stalls this loop, and a stalled loop misses DMX frames.
    if (wr != cur_r || wy != cur_y || wg != cur_g) && now - last_change >= DWELL {
        set_lights(wr, wy, wg);
        cur_r = wr; cur_y = wy; cur_g = wg;
        last_change = now;
    }

    // Absolute pacing. dmx_recv already waited up to STEP and set_lights costs
    // time too; against a fixed target neither accumulates into drift.
    next += STEP;
    if next < now { next = now; }   // fell behind: give up the lost time
    sleep_until(next);
}
