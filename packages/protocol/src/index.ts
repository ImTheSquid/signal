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
  rateLimit: (keyId: string) => `rl:${keyId}`,
  eventsChannel: "events",
} as const;

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
  result: z.enum(["ok", "error", "aborted", "deadline", "preempted", "running"]),
  error: z.string().optional(),
});
export type HistoryEntry = z.infer<typeof HistoryEntrySchema>;

// ---- Pub/sub events (API routes -> WS function) ----

export const EventSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("job"), jobId: z.string() }),
  z.object({ type: z.literal("abort") }),
  z.object({ type: z.literal("idle") }),
]);
export type Event = z.infer<typeof EventSchema>;

// ---- WS messages: server -> device ----

export const JobMsgSchema = z.object({
  t: z.literal("job"),
  id: z.string(),
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
