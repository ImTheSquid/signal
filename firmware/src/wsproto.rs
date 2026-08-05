//! Wire messages, mirroring packages/protocol (zod schemas are authoritative).
//!
//! Reassembly of fragmented frames lives in the `wsframe` crate, which exists so
//! it can carry tests — see the note there.
use serde::{Deserialize, Serialize};

pub use wsframe::JsonFramer;

/// The wire shape, deserialized flat.
///
/// Deliberately NOT an internally-tagged enum. `#[serde(tag = "t")]` cannot know
/// the variant until it has found `t`, so it buffers the **entire** document into
/// a `serde::private::de::Content` tree first. For a 3.4KB job that measured as a
/// large part of a 14.1KB receive transient, against ~33KB free — and on this
/// target a failed allocation aborts and reboots the board. A flat struct streams
/// field by field with no intermediate tree.
///
/// [`ServerMsg`] is the typed form; this exists only to get the bytes in cheaply.
#[derive(Debug, Deserialize)]
pub struct ServerMsgRaw {
    pub t: String,
    // hello
    pub job: Option<JobPayload>,
    pub idle: Option<IdlePayload>,
    // job
    pub id: Option<String>,
    pub holder: Option<String>,
    // job and idle both carry a script at the top level
    pub script: Option<String>,
    pub ttl_ms: Option<u64>,
    /// Rhai components the submitter declared. Absent means all of them, so a
    /// server that never sends this and this firmware still agree.
    pub components: Option<Vec<String>>,
    pub rev: Option<u64>,
}

/// What the rest of the firmware matches on.
#[derive(Debug)]
pub enum ServerMsg {
    Hello {
        job: Option<JobPayload>,
        idle: Option<IdlePayload>,
    },
    Job {
        id: String,
        holder: String,
        script: String,
        ttl_ms: u64,
        components: Option<Vec<String>>,
    },
    Abort,
    Idle {
        script: String,
    },
}

impl TryFrom<ServerMsgRaw> for ServerMsg {
    type Error = String;

    fn try_from(raw: ServerMsgRaw) -> Result<Self, Self::Error> {
        match raw.t.as_str() {
            "hello" => Ok(ServerMsg::Hello {
                job: raw.job,
                idle: raw.idle,
            }),
            "job" => Ok(ServerMsg::Job {
                id: raw.id.ok_or("job without id")?,
                holder: raw.holder.unwrap_or_default(),
                script: raw.script.ok_or("job without script")?,
                ttl_ms: raw.ttl_ms.ok_or("job without ttl_ms")?,
                components: raw.components,
            }),
            "abort" => Ok(ServerMsg::Abort),
            "idle" => Ok(ServerMsg::Idle {
                script: raw.script.ok_or("idle without script")?,
            }),
            other => Err(format!("unknown message type {other:?}")),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct JobPayload {
    pub id: String,
    #[serde(default)]
    pub holder: String,
    pub script: String,
    pub ttl_ms: u64,
    #[serde(default)]
    pub components: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct IdlePayload {
    pub script: String,
}

#[derive(Serialize)]
pub struct LightsJson {
    pub r: bool,
    pub y: bool,
    pub g: bool,
}

#[derive(Serialize)]
#[serde(tag = "t")]
pub enum DeviceMsg<'a> {
    #[serde(rename = "state")]
    State {
        lights: LightsJson,
        running: &'a str,
        heap: u32,
        /// Largest contiguous free block. `heap` alone does not predict an
        /// allocation failure — a fragmented heap with plenty free still cannot
        /// hand out the 32KB contiguous script stack, and on this target a failed
        /// allocation aborts and reboots the board.
        heap_block: u32,
        /// Physical relay transitions per lamp since boot (r, y, g). Mechanical
        /// relays are rated in operations, so this is what says whether a
        /// lighting pattern is affordable.
        ops: [u32; 3],
        fw: &'a str,
    },
    #[serde(rename = "job_done")]
    JobDone {
        id: &'a str,
        result: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
    },
}
