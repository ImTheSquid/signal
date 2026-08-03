// Follow rekordbox lighting on the traffic light.
//
// Three measurements shape this, all taken from a real set:
//
// 1. rekordbox's output is dark ~72% of the time. Treating dark frames as "no
//    data" and falling back to a generic sweep means the light spends most of its
//    time running something that looks exactly like a traffic light — which is
//    the one thing it must not look like. So the last real colour is latched and
//    kept: dark passages keep chasing the most recent colour, never a sweep.
//
// 2. Output is often dim and near-uniform ([3,4,6] is typical), so absolute
//    levels are useless. The ratios carry the colour, so the cut is proportional.
//
// 3. The cut has to be aggressive. At 3/5, near-uniform frames like [2,2,3] put
//    all three lamps in the palette, and a 3-lamp chase over all three lamps is
//    again indistinguishable from a traffic light. At 4/5 the palette is usually
//    one or two lamps, so the chase reads as colour rather than sequence.
//
// The ceiling is hardware: set_lights enforces a 100ms dwell per lamp, so ~10
// transitions/sec/lamp is the limit and TICK sits near it.
//
// Integer math on purpose: floats are enabled in the firmware, but the deployed
// validator wasm predates that and rejects a float literal at submit time.

let MIX_NUM = 4;      // a lamp joins the palette at >= 4/5 of the brightest
let MIX_DEN = 5;
let TICK = 130;       // animation step; lower is busier, bounded by the dwell
let DEAD_MS = 10000;  // no packets at all this long => the bridge itself is gone

// Latched palette. Starts as all three only so there is something to show before
// the first frame arrives; the first real frame replaces it.
let pr = true;
let py = true;
let pg = true;
let seen = false;     // have we ever had a non-dark frame?
let last_pkt = 0;     // last packet of any kind, dark or not
let step = 0;

loop {
    let p = dmx_recv(20);
    let now = millis();

    if p.ok {
        last_pkt = now;
        let r = p.ch[0];
        let g = p.ch[1];
        let b = p.ch[2];

        let m = r;
        if g > m { m = g; }
        if b > m { m = b; }

        // Dark frames deliberately do NOT clear the palette — they would put us
        // back to a sweep for three quarters of the set.
        if m > 0 {
            let cut = (m * MIX_NUM + MIX_DEN - 1) / MIX_DEN;
            // Patched as RGB, so blue drives the yellow lamp.
            pr = r >= cut;
            py = b >= cut;
            pg = g >= cut;
            seen = true;
        }
    }

    step += 1;

    if seen && now - last_pkt < DEAD_MS {
        // Chase inside the latched colour. One lamp drops per phase, so a single
        // colour pulses and a pair alternates — motion either way, and never the
        // three-lamp sequence that reads as a traffic light.
        let phase = step % 3;
        set_lights(pr && phase != 0, py && phase != 1, pg && phase != 2);
    } else {
        // Either nothing has ever arrived, or the bridge has gone quiet. Only
        // here is a generic pattern acceptable, and it is deliberately not a
        // red-yellow-green sequence.
        let phase = step % 4;
        set_lights(phase == 0 || phase == 2, phase == 1, phase == 3);
    }

    sleep(TICK);
}
