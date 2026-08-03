//! Wire messages, mirroring packages/protocol (zod schemas are authoritative).
//!
//! Reassembly of fragmented frames lives in the `wsframe` crate, which exists so
//! it can carry tests — see the note there.
use serde::{Deserialize, Serialize};

pub use wsframe::JsonFramer;

#[derive(Debug, Deserialize)]
#[serde(tag = "t")]
pub enum ServerMsg {
    #[serde(rename = "hello")]
    Hello {
        job: Option<JobPayload>,
        idle: Option<IdlePayload>,
    },
    #[serde(rename = "job")]
    Job {
        id: String,
        #[serde(default)]
        holder: String,
        script: String,
        ttl_ms: u64,
    },
    #[serde(rename = "abort")]
    Abort,
    #[serde(rename = "idle")]
    Idle {
        script: String,
        #[allow(dead_code)] // present on the wire; device state is script-only
        rev: u64,
    },
}

#[derive(Debug, Deserialize)]
pub struct JobPayload {
    pub id: String,
    #[serde(default)]
    pub holder: String,
    pub script: String,
    pub ttl_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct IdlePayload {
    pub script: String,
    #[allow(dead_code)] // present on the wire; device state is script-only
    pub rev: u64,
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
