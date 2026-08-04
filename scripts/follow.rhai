fn pick(pal, i) {
    let out = [false, false, false];
    if i < pal.len() { out[pal[i]] = true; }
    out
}

let STEP = 20;
let PEAK_DECAY = 0.999;
let PEAK_FLOOR = 20.0;
let LAMP_CUT = 0.30;
let DARK = 0.12;
let FLUX_SMOOTH = 0.30;
let THR_SMOOTH = 0.02;
let THR_MULT = 1.8;
let THR_BIAS = 0.03;
let REFRACT = 110;
let BEAT_MIN = 280;
let BEAT_MAX = 1000;
let BEAT_GAP = 3000;
let AGREE_PCT = 22;
let LOOK_BEATS = 8;
let DEAD_MS = 8000;
let PATTERN_MAX = 700;

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

let lr = true;
let ly = true;
let lg = true;

let beat_count = 0;
let last_slot = -1;
let scatter_a = 0;
let scatter_b = -1;

let look = 0;
let look_until = 0;
let accent_until = -99999;

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

        let raw = r;
        if g > raw { raw = g; }
        if b > raw { raw = b; }
        pk_e *= PEAK_DECAY;
        if pk_e < PEAK_FLOOR { pk_e = PEAK_FLOOR; }
        if raw > pk_e { pk_e = raw; }
        let e = raw / pk_e;

        if e > DARK {
            lr = nr >= LAMP_CUT;
            lg = ng >= LAMP_CUT;
            ly = nb >= LAMP_CUT;
            let width = 0;
            if lr { width += 1; }
            if ly { width += 1; }
            if lg { width += 1; }
            if width < 2 {
                lr = true;
                ly = true;
                lg = true;
            }
        }

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
        if pal.len() == 3 {
            let m = rand_int(0, 2);
            if m == 0 { look = 1; } else if m == 1 { look = 4; } else { look = 5; }
        } else {
            look = rand_int(0, 5);
        }
    }

    let wr = false;
    let wy = false;
    let wg = false;

    if now - last_pkt >= DEAD_MS {
        let k = (beat_count / 2) % 3;
        wr = k == 0;
        wy = k == 1;
        wg = k == 2;
    } else if now < accent_until {
        wr = lr; wy = ly; wg = lg;
    } else if look == 0 {
        let on = frac < 500;
        wr = lr && on; wy = ly && on; wg = lg && on;
    } else if look == 1 {
        let s = pick(pal, (beat_count * 2 + frac / 500) % n);
        wr = s[0]; wy = s[1]; wg = s[2];
    } else if look == 2 {
        let on = frac < 250 || (frac >= 500 && frac < 750);
        wr = lr && on; wy = ly && on; wg = lg && on;
    } else if look == 3 {
        let on = (frac >= 500 && frac < 750) || frac >= 875;
        wr = lr && on; wy = ly && on; wg = lg && on;
    } else if look == 4 {
        if lvl > 0.55 {
            let on = quarter % 2 == 0;
            wr = lr && on; wy = ly && on; wg = lg && on;
        } else {
            let s = pick(pal, beat_count % n);
            let on = frac < 500;
            wr = s[0] && on; wy = s[1] && on; wg = s[2] && on;
        }
    } else {
        let s = pick(pal, scatter_a);
        wr = s[0]; wy = s[1]; wg = s[2];
        if scatter_b >= 0 {
            let s2 = pick(pal, scatter_b);
            wr = wr || s2[0]; wy = wy || s2[1]; wg = wg || s2[2];
        }
    }

    if (wr != cur_r || wy != cur_y || wg != cur_g) && now - last_change >= DWELL {
        set_lights(wr, wy, wg);
        cur_r = wr; cur_y = wy; cur_g = wg;
        last_change = now;
    }

    next += STEP;
    if next < now { next = now; }
    sleep_until(next);
}
