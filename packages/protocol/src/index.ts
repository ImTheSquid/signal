import { z } from "zod";

// ---- Shared limits (mirrored in crates/script-env) ----

export const MAX_SCRIPT_BYTES = 16 * 1024;
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
  script: z.string(),
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
  z.object({
    t: z.literal("state"),
    lights: LightsSchema,
    running: z.string(),
    heap: z.number(),
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
