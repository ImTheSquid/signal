#!/usr/bin/env node
// Capture the frames the bridge sends the light, for replay through the script.
//   node dmxcap.mjs [out.txt] [port]        (default dmx.txt, 49500)
//
// The light and this collector both bind 49500; the bridge broadcasts, so both
// receive. Every claim in scripts/README.md comes from a *synthetic* stream —
// this is what a real deck actually sends, and `cargo test -p script-env
// --test follow` replays the file through the real script via FOLLOW_CAPTURE.
//
// One frame per line, `arrival_ms seq base v0,v1,...`. Deliberately not JSON:
// the Rust replay reads it with split_whitespace and awk can answer questions
// about it directly, so neither side needs a parser.

import dgram from "node:dgram";
import fs from "node:fs";
import path from "node:path";

const out = process.argv[2] ?? "dmx.txt";
const port = Number(process.argv[3] ?? 49500);

const MAGIC = 0x544c; // "TL"
const VERSION = 3;
const HEADER_MIN = 10;

const sink = fs.createWriteStream(out, { flags: "a" });
const sock = dgram.createSocket({ type: "udp4", reuseAddr: true });

let t0 = null;
let frames = 0;
let malformed = 0;
let firstSeq = null;
let lastSeq = null;
let sent = 0; // frames the bridge says it sent, from seq
let blank = 0; // frames with every channel at zero
let widest = 0;
const active = new Set(); // channel indices ever non-zero

sock.on("error", (err) => {
  console.error(`bind :${port} failed — ${err.message}`);
  process.exit(1);
});

sock.on("message", (msg) => {
  if (msg.length < HEADER_MIN || msg.readUInt16BE(0) !== MAGIC || msg[2] !== VERSION) {
    malformed += 1;
    return;
  }
  const headerLen = msg[3];
  if (headerLen < HEADER_MIN || msg.length < headerLen) {
    malformed += 1;
    return;
  }
  const seq = msg.readUInt32BE(4);
  const base = msg.readUInt16BE(8);
  const ch = [...msg.subarray(headerLen)];

  const now = Date.now();
  t0 ??= now;
  frames += 1;
  widest = Math.max(widest, ch.length);
  if (ch.every((v) => v === 0)) blank += 1;
  ch.forEach((v, i) => v > 0 && active.add(i));

  // seq counts every packet the bridge *sent*, so the difference against the
  // received count is what the network swallowed — the effect follow.rhai's
  // dseq handling exists to absorb.
  firstSeq ??= seq;
  lastSeq = seq;

  sink.write(`${now - t0} ${seq} ${base} ${ch.join(",")}\n`);
});

sock.bind(port, () => {
  sock.setBroadcast(true);
  console.log(`capturing dmx frames on udp/${port} → ${out} (Ctrl-C to stop)\n`);
});

process.on("SIGINT", () => {
  sink.end();
  const span = t0 === null ? 0 : (Date.now() - t0) / 1000;
  console.log(`\n--- ${frames} frames in ${span.toFixed(1)}s ---`);
  if (malformed) console.log(`${malformed} datagrams did not parse as v${VERSION}`);

  if (frames === 0) {
    console.log("VERDICT: nothing arrived. The bridge is not sending — check that");
    console.log("  ENTTEC DMX-IF is still enabled (it silently drops on re-enumeration)");
    console.log("  and that logcat.mjs on 49510 shows the bridge alive at all.");
    process.exit(0);
  }

  sent = lastSeq - firstSeq + 1;
  const lost = sent - frames;
  console.log(`rate     : ${(frames / span).toFixed(1)}/s received, ${(sent / span).toFixed(1)}/s sent`);
  console.log(`loss     : ${lost} of ${sent} (${((lost / sent) * 100).toFixed(1)}%)`);
  console.log(`channels : ${widest} wide, non-zero at ${[...active].sort((a, b) => a - b).join(",") || "none"}`);
  console.log(`blackout : ${((blank / frames) * 100).toFixed(1)}% of frames all-zero`);

  if (blank === frames) {
    console.log("\nVERDICT: frames arrive but every channel is zero. The lighting engine");
    console.log("is running and outputting blackout — nothing playing, or a venue mismatch.");
  } else {
    console.log(`\nVERDICT: real DMX captured. Replay it with`);
    console.log(
      `  FOLLOW_CAPTURE=${path.resolve(out)} cargo test -p script-env --test follow -- --nocapture`
    );
  }
  process.exit(0);
});
