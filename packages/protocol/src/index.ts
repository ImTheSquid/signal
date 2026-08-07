import { z } from "zod";

/** Rhai standard-library components a script may declare it needs, mirrored from
 *  `Components::NAMES` in crates/script-env.
 *
 *  Declaring is how a script buys heap: the device leaves out what was not
 *  declared, and what it leaves out is room the script's own AST gets instead.
 *  The full set costs roughly half the free heap.
 *
 *  Nothing here can be verified server-side. Rhai resolves calls at run time, so
 *  compiling a script against a narrower engine still succeeds — see the
 *  `unknown_function_compiles` test. An under-declared script fails on the
 *  device, at the first call it cannot resolve. */
export const COMPONENTS = [
  "core",
  "array",
  "map",
  "string",
  "math",
  "iterator",
  "blob",
  "bitfield",
  "functions",
] as const;
export const ComponentsSchema = z.array(z.enum(COMPONENTS));
export type Component = (typeof COMPONENTS)[number];

// ---- Shared limits (mirrored in crates/script-env) ----

/** Size of the script that reaches the device, measured after minification.
 *  The best case — what fits when a script declares the narrowest `components`
 *  it can. Declaring nothing fits less, and the device reports the real limit
 *  with its own numbers when it declines. Sized from what the hardware managed,
 *  which is about 24 bytes of heap per source byte. */
export const MAX_SCRIPT_BYTES = 4 * 1024;
/** Size of a script as submitted. Comments and indentation are stripped before
 *  MAX_SCRIPT_BYTES applies, so this bounds the work one request can ask for. */
export const MAX_RAW_SCRIPT_BYTES = 256 * 1024;
export const WS_MAX_PAYLOAD = 32 * 1024;
/** Device sends a state message at least this often as a WS keepalive. */
export const DEVICE_HEARTBEAT_MS = 20_000;
/** The `device` Redis key expires after this; absence means offline. */
export const DEVICE_KEY_TTL_MS = 90_000;
/** WS function writes the `device` Redis key at most once per interval
 *  unless lights/running changed. */
export const DEVICE_WRITE_MIN_INTERVAL_MS = 60_000;
export const HISTORY_LENGTH = 20;

// ---- Redis keys ----

export const REDIS = {
  lock: "lock",
  jobCurrent: "job:current",
  key: (id: string) => `key:${id}`,
  keys: "keys",
  idle: "idle",
  /** Source map for the idle script. A separate key rather than a field on
   *  `idle`, whose schema is spread straight into the `hello` frame — a map
   *  added there would ride to the device and undo the minification. */
  idleMap: "idle:map",
  device: "device",
  history: "history",
  settings: "settings",
  rateLimit: (keyId: string) => `rl:${keyId}`,
  eventsChannel: "events",
} as const;

export const SettingsSchema = z.object({
  historyPublic: z.boolean().default(true),
});
export type Settings = z.infer<typeof SettingsSchema>;

// ---- Stored JSON shapes ----

export const LockSchema = z.object({
  keyId: z.string(),
  name: z.string(),
  expiresAt: z.number(),
});
export type Lock = z.infer<typeof LockSchema>;

export const JobSchema = z.object({
  jobId: z.string(),
  keyId: z.string(),
  /** Key holder's display name — the device exposes it to idle scripts. */
  holder: z.string().default(""),
  /** Minified. What the device runs, and what `map` resolves positions against. */
  script: z.string(),
  /** Source Map v3 for `script`. Absent when the script was not minified, or was
   *  generated rather than submitted. Server-side only: the `job` frame is built
   *  field by field, so this never reaches the device. Lives here rather than
   *  under its own key so that it expires with the job. */
  map: z.string().optional(),
  /** Submitted size, for reporting what minification saved. */
  rawBytes: z.number().optional(),
  /** Rhai standard-library components the submitter declared this script needs.
   *  Absent means all of them, which is what the device built before scripts
   *  could declare. Each one the device can leave out is heap the script's own
   *  AST gets instead — the full set costs roughly half the free heap. */
  components: z.array(z.string()).optional(),
  /** The script lowered to bytecode, base64. What the device actually runs: it
   *  loads a flat buffer instead of parsing a tree, which on follow.rhai is ~65
   *  allocations rather than ~1069 — and the light's heap charges a header on
   *  every one. Absent falls back to running `script`. */
  artifact: z.string().optional(),
  /** Maps an artifact program counter back to a position in the submitted
   *  source. Server-side only, like `map` and for the same reason: the device
   *  has no use for it, and the `job` frame is built field by field. */
  positions: z.string().optional(),
  expiresAt: z.number(),
});
export type Job = z.infer<typeof JobSchema>;

export const IdleSchema = z.object({
  script: z.string(),
  rev: z.number(),
});
export type Idle = z.infer<typeof IdleSchema>;

export const LightsSchema = z.object({
  r: z.boolean(),
  y: z.boolean(),
  g: z.boolean(),
});
export type Lights = z.infer<typeof LightsSchema>;

export const DeviceStateSchema = z.object({
  lights: LightsSchema,
  running: z.string(), // "idle" | jobId
  heap: z.number(),
  /** Largest contiguous free block. `heap` alone does not predict an allocation
   *  failure: a fragmented heap with plenty free still cannot hand out the 32KB
   *  contiguous script stack, and on that target a failed allocation reboots the
   *  board. Optional so a device on older firmware still validates. */
  heap_block: z.number().optional(),
  /** Physical relay transitions per lamp since boot (r, y, g), for wear
   *  accounting. Optional for the same reason. */
  ops: z.array(z.number()).length(3).optional(),
  fw: z.string(),
  ts: z.number(),
});
export type DeviceState = z.infer<typeof DeviceStateSchema>;

export const HistoryEntrySchema = z.object({
  keyId: z.string(),
  name: z.string(),
  jobId: z.string(),
  start: z.number(),
  end: z.number().nullable(),
  result: z.enum(["ok", "error", "aborted", "deadline", "preempted", "running", "lost"]),
  /** Positions rewritten to the script as submitted, where a map allowed it. */
  error: z.string().optional(),
  /** What the device reported, kept only when remapping changed it. */
  deviceError: z.string().optional(),
  /** Collapsed consecutive runs by the same key (absent = 1). */
  runs: z.number().optional(),
});
export type HistoryEntry = z.infer<typeof HistoryEntrySchema>;

/**
 * Merge adjacent terminal (non-running) entries from the same key into one
 * aggregate row so a single chatty key can't flood the history window.
 * Newest entry of a streak wins (name/jobId/result/end); start stretches to
 * the oldest, `runs` accumulates. Idempotent full-pass; atomic via EVAL.
 * KEYS[1] = history list. Returns number of entries merged away.
 */
export const COLLAPSE_HISTORY_LUA = `
local key = KEYS[1]
local n = redis.call('LLEN', key)
if n < 2 then return 0 end
local entries = {}
for i = 0, n - 1 do
  entries[i + 1] = cjson.decode(redis.call('LINDEX', key, i))
end
local out = {}
for i = 1, n do
  local e = entries[i]
  local last = out[#out]
  if last ~= nil and last.keyId == e.keyId
     and last.result ~= 'running' and e.result ~= 'running' then
    last.runs = (last.runs or 1) + (e.runs or 1)
    if e.start < last.start then last.start = e.start end
    if last.error == nil and e.error ~= nil then last.error = e.error end
  else
    out[#out + 1] = e
  end
end
if #out == n then return 0 end
redis.call('DEL', key)
for i = #out, 1, -1 do
  redis.call('LPUSH', key, cjson.encode(out[i]))
end
return n - #out
`;

// ---- Pub/sub events (API routes -> WS function) ----

export const EventSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("job"), jobId: z.string() }),
  z.object({ type: z.literal("abort") }),
  z.object({ type: z.literal("idle") }),
  // Public status changed for a reason the device doesn't care about
  // (lock acquired, device state written, history edited) — tells the
  // live-dashboard function to push a fresh snapshot.
  z.object({ type: z.literal("update") }),
]);
export type Event = z.infer<typeof EventSchema>;

// ---- WS messages: server -> device ----

export const JobMsgSchema = z.object({
  t: z.literal("job"),
  id: z.string(),
  holder: z.string().default(""),
  script: z.string(),
  ttl_ms: z.number(),
  /** See `JobSchema.components`. Omitted means all of them, so a device on
   *  older firmware and a server that never sends this agree. */
  components: z.array(z.string()).optional(),
  /** See `JobSchema.artifact`. Omitted means the device parses `script`, which
   *  is what firmware without a VM does with it anyway. */
  artifact: z.string().optional(),
});

export const ServerMsgSchema = z.discriminatedUnion("t", [
  z.object({
    t: z.literal("hello"),
    job: JobMsgSchema.omit({ t: true }).nullable(),
    idle: IdleSchema.nullable(),
  }),
  JobMsgSchema,
  z.object({ t: z.literal("abort") }),
  z.object({ t: z.literal("idle"), script: z.string(), rev: z.number() }),
]);
export type ServerMsg = z.infer<typeof ServerMsgSchema>;

// ---- WS messages: device -> server ----

export const DeviceMsgSchema = z.discriminatedUnion("t", [
  // Mirrors DeviceStateSchema minus `ts`, which the server stamps. heap_block
  // and ops are optional so a device on older firmware still validates — but
  // they must be *declared*, or zod strips them and the telemetry silently never
  // reaches Redis.
  z.object({
    t: z.literal("state"),
    lights: LightsSchema,
    running: z.string(),
    heap: z.number(),
    heap_block: z.number().optional(),
    ops: z.array(z.number()).length(3).optional(),
    fw: z.string(),
  }),
  z.object({
    t: z.literal("job_done"),
    id: z.string(),
    result: z.enum(["ok", "error", "aborted", "deadline"]),
    error: z.string().optional(),
  }),
]);
export type DeviceMsg = z.infer<typeof DeviceMsgSchema>;
