//! Wire messages, mirroring packages/protocol (zod schemas are authoritative).
//!
//! Reassembly of fragmented frames lives in the `wsframe` crate, which exists so
//! it can carry tests — see the note there.
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
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
    /// The script already lowered to bytecode, base64'd because this frame is
    /// JSON. Present means run this instead of parsing `script`.
    pub artifact: Option<String>,
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
        artifact: Option<Vec<u8>>,
    },
    Abort,
    Idle {
        script: String,
        components: Option<Vec<String>>,
        artifact: Option<Vec<u8>>,
    },
    Reboot,
}

/// The source, or an empty string when an artifact will be run instead.
///
/// `run_script` loads the artifact and never reads the source when one is
/// present, so the server is free to leave it out. Only a frame carrying
/// neither is unusable.
pub fn script_or_artifact(
    script: Option<String>,
    artifact: &Option<Vec<u8>>,
    what: &str,
) -> Result<String, String> {
    match (script, artifact) {
        (Some(s), _) => Ok(s),
        (None, Some(_)) => Ok(String::new()),
        (None, None) => Err(format!("{what} without script or artifact")),
    }
}

impl TryFrom<ServerMsgRaw> for ServerMsg {
    type Error = String;

    fn try_from(raw: ServerMsgRaw) -> Result<Self, Self::Error> {
        match raw.t.as_str() {
            "hello" => Ok(ServerMsg::Hello {
                job: raw.job,
                idle: raw.idle,
            }),
            "job" => {
                let artifact = decode_artifact(raw.artifact)?;
                Ok(ServerMsg::Job {
                    id: raw.id.ok_or("job without id")?,
                    holder: raw.holder.unwrap_or_default(),
                    script: script_or_artifact(raw.script, &artifact, "job")?,
                    ttl_ms: raw.ttl_ms.ok_or("job without ttl_ms")?,
                    components: raw.components,
                    artifact,
                })
            }
            "abort" => Ok(ServerMsg::Abort),
            "reboot" => Ok(ServerMsg::Reboot),
            "idle" => {
                let artifact = decode_artifact(raw.artifact)?;
                Ok(ServerMsg::Idle {
                    script: script_or_artifact(raw.script, &artifact, "idle")?,
                    components: raw.components,
                    artifact,
                })
            }
            other => Err(format!("unknown message type {other:?}")),
        }
    }
}

/// Turn the base64 an artifact arrives as into the bytes the VM loads.
///
/// A malformed field is refused here rather than passed on: the VM verifies the
/// artifact itself, but a base64 error means the frame is wrong, which is a
/// different failure from a bad program and reads better said so.
pub fn decode_artifact(field: Option<String>) -> Result<Option<Vec<u8>>, String> {
    field
        .map(|text| {
            BASE64
                .decode(text.as_bytes())
                .map_err(|e| format!("job artifact is not valid base64: {e}"))
        })
        .transpose()
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
    #[serde(default)]
    pub artifact: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IdlePayload {
    #[serde(default)]
    pub script: Option<String>,
    /// Absent means the whole standard library, which is what a record written
    /// before idle could declare still says. Declaring narrows the engine from
    /// ~96KB to what the script actually uses.
    #[serde(default)]
    pub components: Option<Vec<String>>,
    #[serde(default)]
    pub artifact: Option<String>,
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
        /// What is filling idle time: `"script"` or `"builtin"`. An idle run
        /// produces no history row — it has no job id — so without this the only
        /// record that the admin script is not running is the serial console.
        idle: &'a str,
        /// Why the idle script is not running, when it is not.
        #[serde(skip_serializing_if = "Option::is_none")]
        idle_error: Option<&'a str>,
    },
    #[serde(rename = "job_done")]
    JobDone {
        id: &'a str,
        result: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
    },
}
