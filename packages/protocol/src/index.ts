import { z } from "zod";

// ---- Shared limits (mirrored in crates/script-env) ----

/** Size of a script as submitted. Bounds the work one request can ask of the
 *  validator; the device's own limit is on the compiled artifact. */
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
  /** As submitted, so positions in errors refer to what the author wrote. */
  script: z.string(),
  /** The script lowered to bytecode, base64. What the device actually runs: it
   *  loads a flat buffer instead of parsing a tree, which on pulse.rhai is ~54
   *  allocations rather than ~756 — and the light's heap charges a header on
   *  every one. Absent falls back to running `script`. */
  artifact: z.string().optional(),
  /** Maps an artifact program counter back to a position in the submitted
   *  source. Server-side only: the device has no use for it, and the `job`
   *  frame is built field by field. */
  positions: z.string().optional(),
  expiresAt: z.number(),
});
export type Job = z.infer<typeof JobSchema>;

export const IdleSchema = z.object({
  script: z.string(),
  rev: z.number(),
  /** See `JobSchema.artifact`. Absent falls back to parsing `script`. */
  artifact: z.string().optional(),
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
  /** What is filling idle time: "script" or "builtin". An idle run has no job
   *  id and so produces no history row — without this, an admin script that is
   *  not running looks exactly like one that is. Optional for the same reason as
   *  the two above. */
  idle: z.enum(["script", "builtin"]).optional(),
  /** Why the idle script is not running, when it is not. */
  idle_error: z.string().optional(),
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
  error: z.string().optional(),
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
  // Deliberately not stored anywhere. Pub/sub is fire-and-forget, so a reboot
  // reaches a device that is connected now and cannot be replayed at one that
  // reconnects afterwards — which is the difference between a reboot and a
  // reboot loop.
  z.object({ type: z.literal("reboot") }),
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
  /** Omitted when `artifact` is present. The device loads the artifact and never
   *  reads the source, so sending both spends receive buffer — the scarcest
   *  thing it has — on bytes it will discard. A frame with neither is refused. */
  script: z.string().optional(),
  ttl_ms: z.number(),
  /** See `JobSchema.artifact`. Omitted means the device parses `script`, which
   *  is what firmware without a VM does with it anyway. */
  artifact: z.string().optional(),
});

/** The idle push. Derived from `IdleSchema` rather than respelled, because the
 *  stored record and the pushed frame are deliberately the same shape — which is
 *  exactly why nothing server-side may live on `IdleSchema`. Spelling them out separately let the two drift silently. */
export const IdleMsgSchema = IdleSchema.extend({
  t: z.literal("idle"),
  /** See `JobMsgSchema.script`: omitted when `artifact` is present. Required in
   *  `IdleSchema` because the stored record is what the artifact is rebuilt
   *  from, and what the admin form edits. */
  script: z.string().optional(),
});

export const ServerMsgSchema = z.discriminatedUnion("t", [
  z.object({
    t: z.literal("hello"),
    job: JobMsgSchema.omit({ t: true }).nullable(),
    // The message shape, not the stored one — `hello` must hand a device the
    // same frame contents the push would.
    idle: IdleMsgSchema.omit({ t: true }).nullable(),
  }),
  JobMsgSchema,
  z.object({ t: z.literal("abort") }),
  IdleMsgSchema,
  z.object({ t: z.literal("reboot") }),
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
    idle: z.enum(["script", "builtin"]).optional(),
    idle_error: z.string().optional(),
  }),
  z.object({
    t: z.literal("job_done"),
    id: z.string(),
    result: z.enum(["ok", "error", "aborted", "deadline"]),
    error: z.string().optional(),
  }),
]);
export type DeviceMsg = z.infer<typeof DeviceMsgSchema>;
