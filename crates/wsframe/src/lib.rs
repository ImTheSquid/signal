//! Reassembling websocket text fragments into whole JSON messages.
//!
//! Its own crate so it can be tested: `firmware` sets `harness = false`, and this
//! is too easy to get subtly wrong to ship untested to a device that needs
//! physical access to reflash.

use std::marker::PhantomData;

use serde::de::DeserializeOwned;

/// Must fit the largest message the server can send, which is `hello`: it
/// carries a job script AND an idle script, each up to `MAX_SCRIPT_BYTES`
/// (16KB), plus envelope and JSON escaping. Measured at 32866 bytes for a
/// two-script hello, so a 16KB-based limit silently discarded the whole message —
/// taking the idle script with it.
pub const DEFAULT_LIMIT: usize = 2 * 16 * 1024 + 4096;

/// Capacity reserved up front and retained between messages.
///
/// Reserved at construction, while the heap is uncontended, so a typical message
/// needs no growth at all later — growth is where the contiguous-allocation risk
/// lives, since `Vec` doubles and must briefly hold both the old and new buffer.
/// Sized above a realistic script (a 3.4KB script is ~3.5KB on the wire).
///
/// Deliberately not the full limit: that is ~36KB (a `hello` can carry two 16KB
/// scripts), and 144KB idle minus 36KB reserved minus ~111KB for engine and stack
/// is negative. Reserving everything would cause the failure it is meant to avoid.
///
/// Also the floor kept after draining: `drain` retains the allocation, so without
/// a shrink one large `hello` would pin tens of KB for the process lifetime.
const KEEP_CAPACITY: usize = 4096;

/// Reassembles websocket text fragments into whole JSON messages.
///
/// # Why this is needed
///
/// The ESP websocket client delivers a message in receive-buffer-sized chunks
/// (~1KB by default), and `esp-idf-svc`'s `WebSocketEventType::Text` is built from
/// `data_len` alone — it discards the `payload_offset` and `payload_len` that say
/// where a message ends. Treating each chunk as a complete document silently
/// dropped every script over ~1KB: measured on hardware, a 410-byte job frame ran
/// and a 1010-byte one never started, while the server accepts up to 16KB.
///
/// # Why framing is left to serde_json
///
/// The obvious fix — counting braces to find the end of a document — means
/// hand-rolling a second JSON parser, and getting the corner cases right (braces
/// inside string literals, escapes) just to rediscover what the real parser
/// already knows. Instead `StreamDeserializer` does the parsing and reports
/// incompleteness itself via [`serde_json::Error::is_eof`], with `byte_offset`
/// saying how much was consumed. No second parser, no guessing, and the message
/// is deserialized once rather than scanned and then parsed.
///
/// The protocol's own length would be better still, but `esp-idf-svc` keeps the
/// raw event private (`new_raw` is not `pub`), so it is not reachable without
/// forking it or driving `esp_websocket_client` directly.
///
/// # Why the buffer is bytes, not a String
///
/// `esp-idf-svc` runs `str::from_utf8` on each chunk of a **text** frame and
/// returns `Err` if a multi-byte character straddles a chunk boundary — the
/// firmware then has no way to recover those bytes, so one lost chunk voids the
/// whole message. A single em dash in a script comment is enough to arm it,
/// depending on where the 1KB boundaries happen to fall.
///
/// Binary frames (op_code 2) are handed over as raw `&[u8]` with no per-chunk
/// validation, so the server sends binary and UTF-8 is validated exactly once,
/// by serde_json, over the reassembled document. Text frames are still accepted
/// so either end can be deployed first.
pub struct JsonFramer<T> {
    buf: Vec<u8>,
    limit: usize,
    _msg: PhantomData<T>,
}

impl<T: DeserializeOwned> JsonFramer<T> {
    pub fn new() -> Self {
        Self::with_limit(DEFAULT_LIMIT)
    }

    /// `limit` bounds the buffer so a malformed stream cannot grow memory
    /// without end.
    pub fn with_limit(limit: usize) -> Self {
        JsonFramer {
            buf: Vec::with_capacity(KEEP_CAPACITY.min(limit)),
            limit,
            _msg: PhantomData,
        }
    }

    /// Drop any partial message. Call on (re)connect: a fragment left from the
    /// previous session would corrupt the first message of the next one.
    pub fn reset(&mut self) {
        self.buf.clear();
        if self.buf.capacity() > KEEP_CAPACITY {
            self.buf.shrink_to(KEEP_CAPACITY);
        }
    }

    /// True when an incomplete message is buffered.
    pub fn is_partial(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Convenience for a text frame. Prefer [`Self::push`]: a text frame is
    /// validated per chunk by esp-idf-svc and can lose a chunk outright.
    pub fn push_str(&mut self, chunk: &str) -> Vec<Result<T, String>> {
        self.push(chunk.as_bytes())
    }

    /// Feed one chunk of bytes, returning every message it completed.
    ///
    /// `Err` is a message that arrived complete but malformed (including invalid
    /// UTF-8, which serde_json reports over the whole document rather than per
    /// chunk); the caller should log it. An incomplete tail is buffered silently
    /// and is not an error.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Result<T, String>> {
        self.buf.extend_from_slice(chunk);

        if self.buf.len() > self.limit {
            let n = self.buf.len();
            self.buf.clear();
            return vec![Err(format!("oversized message discarded ({n} bytes)"))];
        }

        let mut out = Vec::new();
        let mut consumed = 0usize;
        {
            let mut stream = serde_json::Deserializer::from_slice(&self.buf).into_iter::<T>();
            loop {
                match stream.next() {
                    Some(Ok(msg)) => {
                        consumed = stream.byte_offset();
                        out.push(Ok(msg));
                    }
                    // Not an error: the rest of the message hasn't arrived.
                    Some(Err(e)) if e.is_eof() => break,
                    Some(Err(e)) => {
                        // A real syntax error; the stream cannot resynchronise,
                        // so drop everything buffered rather than wedge.
                        out.push(Err(e.to_string()));
                        consumed = self.buf.len();
                        break;
                    }
                    None => {
                        consumed = stream.byte_offset();
                        break;
                    }
                }
            }
        }
        self.buf.drain(..consumed);
        // drain keeps the allocation, so hand the memory back once the buffer is
        // drained. Retaining a hello-sized buffer forever would be most of this
        // device's free heap.
        if self.buf.is_empty() && self.buf.capacity() > KEEP_CAPACITY {
            self.buf.shrink_to(KEEP_CAPACITY);
        }
        out
    }
}

impl<T: DeserializeOwned> Default for JsonFramer<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    /// Shaped like the real `ServerMsg`: a script field full of braces.
    #[derive(Debug, Deserialize, PartialEq)]
    struct Msg {
        t: String,
        #[serde(default)]
        script: String,
    }

    fn ok(v: Vec<Result<Msg, String>>) -> Vec<Msg> {
        v.into_iter().map(|r| r.expect("unexpected error")).collect()
    }

    fn job_doc(script_len: usize) -> String {
        format!(
            r#"{{"t":"job","script":"{}"}}"#,
            "x".repeat(script_len)
        )
    }

    #[test]
    fn single_chunk() {
        let mut f = JsonFramer::new();
        let got = ok(f.push_str(&job_doc(10)));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].t, "job");
    }

    /// The actual failure mode: a 2.4KB job arriving in ~1KB receive buffers.
    #[test]
    fn reassembles_across_receive_buffer_chunks() {
        let doc = job_doc(2400);
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        let mut got = Vec::new();
        for chunk in doc.as_bytes().chunks(1024) {
            got.extend(ok(f.push_str(std::str::from_utf8(chunk).unwrap())));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].script.len(), 2400);
        assert!(!f.is_partial());
    }

    #[test]
    fn reassembles_one_byte_at_a_time() {
        let doc = job_doc(200);
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        let mut got = Vec::new();
        for ch in doc.chars() {
            got.extend(ok(f.push_str(&ch.to_string())));
        }
        assert_eq!(got.len(), 1);
    }

    /// The case a brace counter has to special-case and this does not: a Rhai
    /// script lives inside a JSON string and is full of braces.
    #[test]
    fn braces_inside_string_literals_are_not_structure() {
        let doc = r#"{"t":"idle","script":"loop { set_lights(true,false,false); }"}"#;
        let mut f = JsonFramer::new();
        let got = ok(f.push_str(doc));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].script, "loop { set_lights(true,false,false); }");
    }

    #[test]
    fn split_inside_a_string_literal_containing_braces() {
        let doc = r#"{"t":"idle","script":"loop { sleep(1); }"}"#;
        let cut = doc.find("sleep").unwrap();
        let mut f = JsonFramer::new();
        assert!(ok(f.push_str(&doc[..cut])).is_empty());
        let got = ok(f.push_str(&doc[cut..]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].script, "loop { sleep(1); }");
    }

    #[test]
    fn escaped_quote_does_not_end_the_string() {
        let doc = r#"{"t":"idle","script":"a \" { b"}"#;
        let mut f = JsonFramer::new();
        let got = ok(f.push_str(doc));
        assert_eq!(got[0].script, r#"a " { b"#);
    }

    #[test]
    fn two_messages_in_one_chunk() {
        let mut f = JsonFramer::new();
        let got = ok(f.push_str(r#"{"t":"abort"}{"t":"abort"}"#));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn whitespace_between_messages_is_ignored() {
        let mut f = JsonFramer::new();
        let got = ok(f.push_str("  {\"t\":\"abort\"}\n\n  {\"t\":\"abort\"}  "));
        assert_eq!(got.len(), 2);
        assert!(!f.is_partial());
    }

    #[test]
    fn nested_objects_only_complete_at_the_end() {
        #[derive(Debug, Deserialize)]
        struct Hello {
            t: String,
            job: Option<Msg>,
        }
        let doc = r#"{"t":"hello","job":{"t":"job","script":"x"}}"#;
        let mut f: JsonFramer<Hello> = JsonFramer::new();
        let got = f.push_str(doc);
        assert_eq!(got.len(), 1);
        let h = got.into_iter().next().unwrap().unwrap();
        assert_eq!(h.t, "hello");
        assert!(h.job.is_some());
    }

    /// A complete but malformed message is reported, not buffered forever.
    #[test]
    fn malformed_message_is_reported_and_cleared() {
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        let got = f.push_str(r#"{"t":}"#);
        assert_eq!(got.len(), 1);
        assert!(got[0].is_err());
        assert!(!f.is_partial(), "buffer must not wedge on bad input");
        assert_eq!(ok(f.push_str(r#"{"t":"abort"}"#)).len(), 1);
    }

    #[test]
    fn oversized_message_is_dropped_and_framer_recovers() {
        let mut f: JsonFramer<Msg> = JsonFramer::with_limit(256);
        let got = f.push_str(&format!("{{{}", "y".repeat(400)));
        assert!(got[0].is_err());
        assert_eq!(ok(f.push_str(r#"{"t":"abort"}"#)).len(), 1);
    }

    /// The em-dash bug, reproduced. A multi-byte character straddling a chunk
    /// boundary must survive. Over a text frame esp-idf-svc rejects that chunk
    /// before the framer ever sees it, which voids the whole message; over bytes
    /// there is nothing to reject, and serde_json validates the joined document.
    #[test]
    fn multibyte_char_split_across_chunks_survives() {
        // Three em dashes, exactly as a script comment would carry them. U+2014
        // is three bytes, so there are two interior split points per dash.
        let dash = '\u{2014}';
        let pad = "x".repeat(300);
        let doc = format!(r#"{{"t":"idle","script":"a {dash} b {pad} c {dash} d {dash} e"}}"#);
        let bytes = doc.as_bytes();
        assert!(!doc.is_ascii(), "test must actually contain multi-byte chars");

        // Split at every byte offset; every split must still yield one message.
        for cut in 1..bytes.len() {
            let (head, tail) = bytes.split_at(cut);
            // Confirm the split is genuinely mid-character somewhere in the run.
            let mut f: JsonFramer<Msg> = JsonFramer::new();
            let mut got = f.push(head);
            got.extend(f.push(tail));
            let msgs: Vec<_> = got.into_iter().collect();
            assert_eq!(
                msgs.len(),
                1,
                "split at byte {cut} produced {} results",
                msgs.len()
            );
            assert!(msgs[0].is_ok(), "split at byte {cut}: {:?}", msgs[0]);
        }
    }

    /// Invalid UTF-8 must be a reported error over the whole document, not a
    /// silently dropped chunk.
    #[test]
    fn invalid_utf8_is_reported_not_dropped() {
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        let mut bad = br#"{"t":"idle","script":"#.to_vec();
        bad.extend_from_slice(&[0x22, 0xff, 0xfe, 0x22, 0x7d]); // "<bad>"}
        let out = f.push(&bad);
        assert!(out.iter().any(|r| r.is_err()), "must report, got {out:?}");
        assert!(!f.is_partial(), "must not wedge");
    }

    /// `hello` carries a job script AND an idle script, each up to 16KB. A limit
    /// sized for one script discarded the whole message, losing the idle script
    /// with it.
    #[test]
    fn accepts_a_hello_carrying_two_maximal_scripts() {
        #[derive(Debug, Deserialize)]
        struct Hello {
            t: String,
            job: Option<Msg>,
            idle: Option<Msg>,
        }
        let big = "s".repeat(16 * 1024);
        let doc = format!(
            r#"{{"t":"hello","job":{{"t":"job","script":"{big}"}},"idle":{{"t":"idle","script":"{big}"}}}}"#
        );
        assert!(doc.len() > 32_000, "doc is {} bytes", doc.len());

        let mut f: JsonFramer<Hello> = JsonFramer::new();
        let mut got = Vec::new();
        for chunk in doc.as_bytes().chunks(1024) {
            got.extend(f.push_str(std::str::from_utf8(chunk).unwrap()));
        }
        assert_eq!(got.len(), 1, "a two-script hello must not be discarded");
        let h = got.into_iter().next().unwrap().expect("must parse");
        assert_eq!(h.t, "hello");
        assert!(h.job.is_some() && h.idle.is_some());
    }

    /// Reserved at construction, so a typical script needs no growth at all —
    /// growth is where the contiguous-allocation risk lives.
    #[test]
    fn capacity_is_reserved_up_front() {
        let f: JsonFramer<Msg> = JsonFramer::new();
        assert!(
            f.buf.capacity() >= 4096,
            "reserved only {}",
            f.buf.capacity()
        );
    }

    #[test]
    fn a_typical_script_needs_no_growth() {
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        let before = f.buf.capacity();
        // A 3.4KB script, the size actually submitted, in 1KB chunks.
        let doc = job_doc(3400);
        for chunk in doc.as_bytes().chunks(1024) {
            f.push(chunk);
        }
        assert_eq!(
            f.buf.capacity(),
            before,
            "buffer grew for a message that should fit the reservation"
        );
    }

    /// A large message must not pin its buffer for the process lifetime: `drain`
    /// keeps the allocation, and this device has ~33KB free while a script runs.
    #[test]
    fn capacity_is_released_after_a_large_message() {
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        let doc = job_doc(20_000);
        assert_eq!(ok(f.push_str(&doc)).len(), 1);
        assert!(
            f.buf.capacity() <= KEEP_CAPACITY,
            "retained {} bytes of capacity",
            f.buf.capacity()
        );
    }

    /// The network-drop case. A partial is buffered, the connection dies, and a
    /// later message arrives concatenated onto it. The result is malformed, so
    /// the framer must report it and clear — losing one message, not wedging.
    #[test]
    fn partial_then_unrelated_message_recovers() {
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        assert!(f.push_str(r#"{"t":"job","scr"#).is_empty());
        assert!(f.is_partial());

        let out = f.push_str(r#"{"t":"abort"}"#);
        assert!(
            out.iter().any(|r| r.is_err()),
            "a malformed concatenation must be reported"
        );
        assert!(!f.is_partial(), "must not stay wedged");

        // And the very next message works.
        assert_eq!(ok(f.push_str(r#"{"t":"abort"}"#)).len(), 1);
    }

    /// A partial that is cut mid-string-literal is the nastiest shape, because the
    /// next message's braces and quotes are consumed as string contents.
    #[test]
    fn partial_inside_string_then_new_message_recovers() {
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        assert!(f.push_str(r#"{"t":"idle","script":"loop { sle"#).is_empty());
        let out = f.push_str(r#"{"t":"abort"}"#);
        assert!(out.iter().any(|r| r.is_err()) || out.is_empty());
        // Either it errored and cleared, or it is still buffering; what must NOT
        // happen is being stuck forever. Force the session boundary and confirm.
        f.reset();
        assert_eq!(ok(f.push_str(r#"{"t":"abort"}"#)).len(), 1);
    }

    /// A stale partial pins memory until something clears it. This documents how
    /// much: the caller must reset on disconnect, not only on connect.
    #[test]
    fn stale_partial_holds_memory_until_reset() {
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        let big = format!(r#"{{"t":"job","script":"{}"#, "x".repeat(8000));
        assert!(f.push_str(&big).is_empty());
        assert!(f.is_partial(), "8KB is held indefinitely with no reset");
        f.reset();
        assert!(!f.is_partial());
    }

    #[test]
    fn reset_discards_a_partial_message() {
        let mut f: JsonFramer<Msg> = JsonFramer::new();
        f.push_str(r#"{"t":"jo"#);
        assert!(f.is_partial());
        f.reset();
        assert!(!f.is_partial());
        assert_eq!(ok(f.push_str(r#"{"t":"abort"}"#)).len(), 1);
    }
}
