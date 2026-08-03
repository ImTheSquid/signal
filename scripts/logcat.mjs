#!/usr/bin/env node
// Collector for the bridge's UDP log sink.
//   node logcat.mjs [port]        (default 49510)
// Prints every datagram with a timestamp and the sender, so "did wifi logging
// actually work" is answerable without the USB console.

import dgram from "node:dgram";

const port = Number(process.argv[2] ?? 49510);
const sock = dgram.createSocket({ type: "udp4", reuseAddr: true });
let count = 0;

sock.on("error", (err) => {
  console.error(`bind :${port} failed — ${err.message}`);
  process.exit(1);
});

sock.on("message", (msg, rinfo) => {
  count += 1;
  const t = new Date().toISOString().slice(11, 23);
  console.log(`${t}  ${rinfo.address.padEnd(15)} ${msg.toString("utf8").trimEnd()}`);
});

sock.bind(port, () => {
  sock.setBroadcast(true);
  console.log(`listening for bridge logs on udp/${port} (Ctrl-C to stop)\n`);
});

process.on("SIGINT", () => {
  console.log(`\n--- ${count} log lines received ---`);
  console.log(count === 0 ? "VERDICT: nothing arrived." : "VERDICT: UDP logging works.");
  process.exit(0);
});
