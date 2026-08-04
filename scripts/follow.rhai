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
let PEAK_DECAY = 0.999;
let PEAK_FLOOR = 20.0;
let DARK = 0.10;
let FLUX_SMOOTH = 0.30;
let THR_SMOOTH = 0.02;
let THR_MULT = 1.8;
let THR_BIAS = 0.03;
let REFRACT = 110;
let BEAT_MIN = 380;
let BEAT_MAX = 680;
let BEAT_GAP = 6000;
let AGREE_PCT = 25;
let LOOK_BEATS = 8;
let DEAD_MS = 8000;
let PATTERN_MAX = 700;
let DIMMER_CH = 4;
let WIDE_AT = 0.30;
let FULL_AT = 0.65;

let DWELL = lamp_dwell_ms();
if DWELL < 1 { DWELL = 1; }

let pk_r = PEAK_FLOOR;
let pk_g = PEAK_FLOOR;
let pk_b = PEAK_FLOOR;
let pk_e = PEAK_FLOOR;
let e_prev = 0.0;
let flux = 0.0;
let thr = 0.05;
let lvl = 0.0;

let last_onset = -99999;
let period = 500;
let anchor = 0;
let disagree = 0;

let ord = [0, 1, 2];

let beat_count = 0;
let last_slot = -1;
let scatter_start = 0;
let scatter_w = 1;

let look = 0;
let look_until = 0;
let accent_until = -99999;

let cur_r = false;
let cur_y = false;
let cur_g = false;
let last_change = -99999;

let nudge = false;
let nudge_s = [false, false, false];

let t0 = millis();
let next = t0;
let last_pkt = t0;
anchor = t0;

loop {
    let p = dmx_recv(STEP);
    let now = millis();

    if p.ok {
        last_pkt = now;

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

        ord = order3(r / pk_r, b / pk_b, g / pk_g);

        let raw = r;
        if g > raw { raw = g; }
        if b > raw { raw = b; }
        if p.ch.len() > DIMMER_CH { raw = p.ch[DIMMER_CH].to_float(); }
        pk_e *= PEAK_DECAY;
        if pk_e < PEAK_FLOOR { pk_e = PEAK_FLOOR; }
        if raw > pk_e { pk_e = raw; }
        let e = raw / pk_e;

        let d = e - e_prev;
        if d < 0.0 { d = 0.0; }
        e_prev = e;
        flux += (d - flux) * FLUX_SMOOTH;
        lvl += (e - lvl) * 0.06;

        if flux > thr * THR_MULT + THR_BIAS && now - last_onset >= REFRACT {
            let iv = now - last_onset;
            last_onset = now;
            accent_until = now + DWELL;

            if iv <= BEAT_GAP {
                while iv < BEAT_MIN { iv *= 2; }
                while iv > BEAT_MAX { iv /= 2; }
                if iv >= BEAT_MIN {
                    let err = iv - period;
                    if err < 0 { err = -err; }
                    if err * 100 <= period * AGREE_PCT {
                        period = (period * 4 + iv) / 5;
                        anchor = now;
                        disagree = 0;
                    } else {
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
        thr += (flux - thr) * THR_SMOOTH;
    }

    let pd = period;
    while pd > PATTERN_MAX { pd /= 2; }
    while pd < DWELL * 4 { pd *= 2; }

    let since = now - anchor;
    if since < 0 { since = 0; }
    let frac = (since % pd) * 1000 / pd;
    let quarter = frac / 250;
    let half = frac / 500;

    let w = 1;
    if lvl > WIDE_AT { w = 2; }
    if lvl > FULL_AT { w = 3; }

    if quarter != last_slot {
        if quarter == 0 { beat_count += 1; }
        nudge = false;
        if quarter % 2 == 0 {
            scatter_start = rand_int(0, 2);
            scatter_w = rand_int(0, w);
            if scatter_w == 0 && rand_chance(0.6) { scatter_w = 1; }
        }
        last_slot = quarter;
    }

    if beat_count >= look_until {
        look_until = beat_count + LOOK_BEATS;
        look = rand_int(0, 5);
    }

    let s = [false, false, false];

    if now - last_pkt >= DEAD_MS {
        s = pickn(ord, beat_count, 1 + (beat_count / 3) % 2);
    } else if now < accent_until {
        s = pickn(ord, 0, 3);
    } else if look == 0 {
        if quarter < 2 { s = pickn(ord, 0, w); }
    } else if look == 1 {
        s = pickn(ord, beat_count * 2 + half, 1);
    } else if look == 2 {
        if quarter == 0 || quarter == 2 { s = pickn(ord, 0, w); }
    } else if look == 3 {
        if quarter >= 2 { s = pickn(ord, half, w); }
    } else if look == 4 {
        let k = quarter + 1;
        if k > 3 { k = 0; }
        if k > w + 1 { k = w + 1; }
        s = pickn(ord, 0, k);
    } else {
        s = pickn(ord, scatter_start, scatter_w);
    }

    if !nudge && now - last_change >= pd / 2 {
        nudge = true;
        if cur_r || cur_y || cur_g {
            nudge_s = [false, false, false];
        } else {
            let k = w;
            if k < 1 { k = 1; }
            nudge_s = pickn(ord, beat_count, k);
        }
    }
    if nudge { s = nudge_s; }

    if (s[0] != cur_r || s[1] != cur_y || s[2] != cur_g) && now - last_change >= DWELL {
        set_lights(s[0], s[1], s[2]);
        cur_r = s[0]; cur_y = s[1]; cur_g = s[2];
        last_change = now;
    }

    next += STEP;
    if next < now { next = now; }
    sleep_until(next);
}
