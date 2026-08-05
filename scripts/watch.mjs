#!/usr/bin/env node
// Record what the light actually did, from the public live socket.
//   node watch.mjs [out.jsonl] [wss://host/api/live]   (default light.jsonl)
//
// The device reports `lights`, `running` and per-lamp relay `ops`, so the real
// lamp timeline is recoverable without touching the firmware. Run this next to
// dmxcap.mjs through a set: dmxcap says what the light was told, this says what
// it did, and the gap between them is the whole diagnosis.
//
// `ops` counts since boot and the light is shared, so a total says nothing about
// any one script. Everything below is therefore attributed per job: segments are
// cut whenever `running` changes, and only the job you were watching is judged.

import fs from "node:fs";

const out = process.argv[2] ?? "light.jsonl";
const url = process.argv[3] ?? "wss://signal.jackhogan.me/api/live";

const sink = fs.createWriteStream(out, { flags: "a" });

let t0 = null;
let snapshots = 0;
let offline = 0;
const errors = [];
const jobs = new Map(); // running -> { name, ops:[3], changes, ms, segments }
let seg = null; // current contiguous run of one `running` value

function closeSegment() {
  if (!seg) return;
  const j = jobs.get(seg.running) ?? {
    name: seg.name,
    ops: [0, 0, 0],
    changes: 0,
    ms: 0,
    segments: 0,
  };
  if (seg.firstOps && seg.lastOps) {
    for (let i = 0; i < 3; i++) j.ops[i] += seg.lastOps[i] - seg.firstOps[i];
  }
  j.changes += seg.changes;
  j.ms += seg.lastT - seg.firstT;
  j.segments += 1;
  if (seg.name) j.name = seg.name;
  jobs.set(seg.running, j);
  seg = null;
}

function record(snap) {
  const now = Date.now();
  t0 ??= now;
  snapshots += 1;
  sink.write(`${JSON.stringify({ t: now - t0, ...snap })}\n`);

  const d = snap.device;
  if (!d) {
    offline += 1;
    return;
  }

  if (!seg || seg.running !== d.running) {
    closeSegment();
    seg = {
      running: d.running,
      name: snap.lock?.holder ?? null,
      firstOps: d.ops ?? null,
      lastOps: d.ops ?? null,
      changes: 0,
      prev: null,
      firstT: now,
      lastT: now,
    };
  }
  seg.name ??= snap.lock?.holder ?? null;
  seg.lastT = now;
  if (d.ops) {
    seg.firstOps ??= d.ops;
    seg.lastOps = d.ops;
  }

  const key = `${d.lights.r}${d.lights.y}${d.lights.g}`;
  if (seg.prev !== null && key !== seg.prev) seg.changes += 1;
  seg.prev = key;

  for (const h of snap.history ?? []) {
    if ((h.result === "error" || h.result === "lost") && !errors.some((e) => e.jobId === h.jobId)) {
      errors.push(h);
    }
  }
}

function connect() {
  const ws = new WebSocket(url);
  ws.addEventListener("message", (ev) => {
    try {
      record(JSON.parse(ev.data));
    } catch (e) {
      console.error(`bad snapshot: ${e.message}`);
    }
  });
  ws.addEventListener("open", () => console.log(`watching ${url} → ${out} (Ctrl-C to stop)\n`));
  // Redeploys drop the socket; the set outlasts them.
  ws.addEventListener("close", () => setTimeout(connect, 2000));
  ws.addEventListener("error", () => {});
}

connect();

process.on("SIGINT", () => {
  closeSegment();
  sink.end();
  const span = t0 === null ? 0 : (Date.now() - t0) / 1000;
  console.log(`\n--- ${snapshots} snapshots over ${span.toFixed(1)}s ---`);

  if (snapshots === 0) {
    console.log("VERDICT: no snapshots. The live socket never delivered — check the URL.");
    process.exit(0);
  }
  if (offline === snapshots) {
    console.log("VERDICT: the device was offline for the whole capture.");
    process.exit(0);
  }

  console.log("\njob        holder            secs  changes   relay ops r/y/g");
  for (const [running, j] of jobs) {
    const id = running === "idle" ? "idle" : running.slice(0, 8);
    console.log(
      `${id.padEnd(10)} ${(j.name ?? "-").padEnd(16)} ${(j.ms / 1000).toFixed(0).padStart(5)} ` +
        `${String(j.changes).padStart(8)}   ${j.ops.join(" / ")}`
    );
  }

  for (const e of errors) {
    console.log(`\n  ${e.jobId.slice(0, 8)} ${e.result}: ${e.error ?? "(no message)"}`);
  }

  // Only jobs that actually drove the lamps can say anything about a lamp.
  const driving = [...jobs].filter(([r, j]) => r !== "idle" && j.ops.some((v) => v > 0));
  const dark = driving.filter(([, j]) => j.ops[1] === 0 && (j.ops[0] > 0 || j.ops[2] > 0));

  console.log();
  if (driving.length === 0) {
    console.log("VERDICT: no job moved a relay during this capture. Whatever you were");
    console.log("watching, it was not a script driving the lamps — check `running` above.");
  } else if (dark.length > 0) {
    console.log(`VERDICT: yellow never switched under ${dark.map(([r]) => r.slice(0, 8)).join(", ")}`);
    console.log("while red or green did. follow.rhai cannot do that — its chase walks all");
    console.log("three positions and its dead-DMX branch lights yellow two thirds of the");
    console.log("time. Confirm that job was running follow.rhai and not something else.");
  } else {
    console.log("VERDICT: every driving job moved all three lamps. The complaint is not");
    console.log("reproducible as a stuck lamp — compare the timeline against the simulation");
    console.log("of the same frames (see dmxcap.mjs's FOLLOW_CAPTURE line).");
  }
  process.exit(0);
});
