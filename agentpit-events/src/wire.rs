//! NDJSON wire protocol for daemon⇔client and worker⇔client conversations (design §5.1).
//!
//! One JSON object per line, LF-delimited. Requests carry an `id` echoed by the matching
//! response; `event` frames are unsolicited pushes from a worker to its attached clients.
//! Deliberately NOT JSON-RPC: framing + id correlation is the entire requirement, and the
//! tagged-enum encoding matches every other agentpit wire format.
//!
//! The daemon is a CONTROL PLANE broker: clients ask it to ensure/create workers and get
//! back the worker's socket path, then talk to the worker directly (data plane). This
//! deviates from prime-agent's supervisor-proxies-everything design on purpose — every
//! user-visible property (detach never touches the loop, one daemon per user, crash
//! recovery) survives, with one hop less plumbing.
//!
//! The types live here rather than in the CLI so every speaker — the CLI's daemon, workers
//! and clients, and the dashboard — shares one definition.
//!
//! Compatibility rules (a running daemon outlives the binary that talks to it, so both
//! directions of version skew are normal):
//! - New fields are additive: `#[serde(default)]` on read and skipped on write while empty,
//!   so an older peer's frames still parse and a frame that does not use the field is
//!   byte-identical to what an older peer would send.
//! - New variants land in each enum's trailing `Unknown` variant: an unknown request is
//!   answered with an [`CODE_UNSUPPORTED`] error, an unknown response kind or event is
//!   ignored. Neither is ever acted on.
//! - Only a breaking change bumps [`PROTO_VERSION`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::loops::{Cursor, LoopSummary, OpRequest, OpResult, Origin};

/// Bumped on breaking wire changes; checked in the `hello` handshake by BOTH sides.
pub const PROTO_VERSION: u32 = 1;

/// [`Response::code`] for a line that did not parse as a [`Request`].
pub const CODE_BAD_REQUEST: &str = "bad_request";
/// [`Response::code`] for a request type the server does not know (a newer peer's verb).
pub const CODE_UNSUPPORTED: &str = "unsupported";

/// The `hello.features` entry for blueprint loops: the daemon's `loop_*` verbs and the loop
/// runner's socket (docs/workspace-loop-design.md §11).
pub const FEATURE_LOOPS: &str = "loops/1";

/// `hello.role` of a loop runner.
pub const ROLE_LOOP: &str = "loop";

/// A request frame: `id` is caller-chosen and echoed in the response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub body: RequestBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestBody {
    /// First frame on every connection, both daemon- and worker-side.
    Hello {
        proto: u32,
        /// Optional capabilities this client understands (e.g. `"loops/1"`). Empty = none,
        /// and then absent from the wire.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
        /// Free-form client identity for diagnostics (e.g. `"agentpit/0.2.18"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },

    // ── daemon (control plane) ────────────────────────────────────────────────
    /// Create a fresh session and spawn its worker.
    Create { cwd: String },
    /// Ensure a worker for an existing session (spawn/rehydrate as needed).
    Ensure { session: String },
    /// List sessions with live state (running/idle from workers, inactive from disk).
    List,
    /// Stop one session's worker (graceful; refuses while an exchange is running unless
    /// `force`).
    StopWorker { session: String, force: bool },
    /// Stop the daemon itself. Workers keep running unless `all`.
    Shutdown { all: bool },

    // ── worker (data plane) ──────────────────────────────────────────────────
    /// Subscribe this connection to events and get a transcript snapshot.
    Attach { tail: usize },
    /// Run one conversational turn. Rejected with `busy` while another is in flight.
    Send {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend: Option<String>,
    },
    /// Unsubscribe (bookkeeping only — never touches the running loop, §5.3).
    Detach,
    /// The session tree as display lines (P1's /tree, served remotely).
    Tree,
    /// Move the leaf; optionally record a summary of the branch being left (B5).
    Branch {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// Fork at `at` (or the leaf) into a new session file; returns the new id.
    Fork {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<String>,
    },
    /// Summarize + fold history (the /compact verb, run inside the worker).
    Compact,
    /// Cancel the in-flight turn, if any (the remote Ctrl-C).
    Cancel,
    /// Run one orchestration-REPL cell (TypeScript) in the session's deno sidecar
    /// (design §10). Serialized with turns via the same busy flag.
    ReplCell { code: String },
    /// Cheap liveness/state probe.
    Status,

    // ── daemon: blueprint loops (feature `loops/1`) ─────────────────────────────
    /// Create a loop (idempotent per `op_id`, a canonical UUID the loop id derives from)
    /// and ensure its runner.
    LoopStart {
        op_id: String,
        blueprint: BlueprintSource,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        inputs: BTreeMap<String, String>,
        /// Absolute working directory the loop runs in.
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Start running at once (otherwise the loop waits in `created` for a `start` op).
        #[serde(default = "default_true")]
        start: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<Origin>,
    },
    /// Ensure a live runner for an unfinished loop and return its socket.
    LoopEnsure { loop_id: String },
    /// Every loop's summary (from `head.json`; no runner is contacted).
    LoopList {
        #[serde(default)]
        include_terminal: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// Stop a loop's runner (graceful; refused while compute steps run unless `force`).
    /// The loop itself is not stopped: the next `loop_ensure` resumes it.
    LoopStopRunner {
        loop_id: String,
        #[serde(default)]
        force: bool,
    },

    // ── loop runner (feature `loops/1`) ─────────────────────────────────────────
    /// Subscribe to the loop's journal: `loop_attached` comes back first, then every record
    /// after `since` (all of them without a cursor, or after a reset) as `loop_record`
    /// frames, then live records as they are written.
    LoopAttach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<Cursor>,
        /// Also stream `loop_chunk` frames (live agent output; lossy).
        #[serde(default)]
        chunks: bool,
    },
    /// The loop's board row.
    LoopStatus,
    /// A byte range of one step's file.
    LoopRead {
        what: LoopFile,
        step_id: String,
        #[serde(default)]
        offset: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
    },
    /// An operation on the loop (design §7). Answered with `op_result` AFTER the records it
    /// caused, on the same connection.
    LoopOp(OpRequest),

    /// A newer peer's request type this build does not know. Only ever produced by
    /// deserialization: servers answer it with a [`CODE_UNSUPPORTED`] error and never act
    /// on it. Its fields are dropped, so it is not meant to be sent.
    #[serde(other)]
    Unknown,
}

fn default_true() -> bool {
    true
}

/// Where `loop_start` takes the blueprint from. The daemon freezes the document into the
/// journal, so the source is never read again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum BlueprintSource {
    /// A file path (absolute, or relative to the request's `cwd`).
    Path { path: String },
    /// The document itself.
    Inline { doc: serde_json::Value },
    /// `<cwd>/.agentpit/blueprints/<name>.json`, then `~/.config/agentpit/blueprints/<name>.json`.
    Named { name: String },
    /// A newer client's source kind; answered `unsupported`.
    #[serde(other)]
    Unknown,
}

/// Which per-step file `loop_read` returns.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoopFile {
    /// The rendered prompt (`prompts/<step>.md`).
    Prompt,
    /// The live output as streamed (`outputs/<step>.log`).
    Output,
    /// The final answer (`outputs/<step>.md`).
    Answer,
    /// A check's combined stdout/stderr (`checks/<step>.log`).
    CheckLog,
    #[serde(other)]
    Unknown,
}

/// One loop in `loop_list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoopRow {
    pub summary: LoopSummary,
    /// `"live"` (a runner is registered and alive) or `"absent"`.
    pub runner: String,
}

/// A response frame: `ok:true` carries `data`, `ok:false` carries `error` (plus, from newer
/// servers, a machine-readable `code` and optional `details`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ResponseData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Machine-readable error class ([`CODE_BAD_REQUEST`], [`CODE_UNSUPPORTED`], …) so a
    /// client can branch without matching on `error` prose. `None` on success and on every
    /// error from an older server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Structured context for `code`, when there is any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(id: u64, data: ResponseData) -> Self {
        Response {
            id,
            ok: true,
            data: Some(data),
            error: None,
            code: None,
            details: None,
        }
    }

    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Response {
            id,
            ok: false,
            data: None,
            error: Some(error.into()),
            code: None,
            details: None,
        }
    }

    /// An error carrying a machine-readable `code` next to the human-readable message.
    pub fn err_code(id: u64, code: &str, error: impl Into<String>) -> Self {
        Response {
            code: Some(code.to_string()),
            ..Response::err(id, error)
        }
    }

    /// Attach structured context (usually to an error built by [`Response::err_code`]).
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// The reply to a `line` that did not parse as a [`Request`]. Echoes the line's numeric
    /// `id` when it has one (0 otherwise), so a client waiting on that id gets its answer
    /// instead of hanging on a response that will never carry it.
    pub fn bad_request(line: &str, error: impl std::fmt::Display) -> Self {
        let id = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("id").and_then(serde_json::Value::as_u64))
            .unwrap_or(0);
        Response::err_code(id, CODE_BAD_REQUEST, format!("bad request: {error}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseData {
    Hello {
        proto: u32,
        /// "daemon" | "worker" — lets a client detect a socket-path mixup immediately.
        role: String,
        pid: u32,
        /// Optional capabilities this server offers (e.g. `"loops/1"`), so a client can
        /// gate optional verbs up front instead of probing for `unsupported`. Empty = none,
        /// and then absent from the wire.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
    },
    Session {
        session_id: String,
        socket: String,
    },
    Sessions {
        sessions: Vec<SessionRow>,
    },
    Snapshot {
        session_id: String,
        /// (who, text) pairs — same shape as `SessionRecorder::context_items`.
        transcript: Vec<(String, String)>,
        total_entries: usize,
        shown: usize,
    },
    Turn {
        status: String,
        answer: String,
    },
    Lines {
        lines: Vec<String>,
    },
    Forked {
        session_id: String,
    },
    /// A REPL cell's ending: `check_error` = refused before execution (§10.5),
    /// `error` = threw at runtime, otherwise `repr` displays the returned value.
    Cell {
        ok: bool,
        repr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default)]
        check_failed: bool,
    },
    WorkerStatus {
        session_id: String,
        busy: bool,
        attached_clients: usize,
        /// Milliseconds since the last recorded activity (turn start/end).
        idle_ms: u64,
    },
    Unit,

    /// `loop_start`: the loop (new, or the one this `op_id` already created) and its
    /// runner's socket (absent when the loop has already finished).
    LoopStarted {
        loop_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        socket: Option<String>,
        duplicate: bool,
    },
    /// `loop_ensure`.
    LoopRunner {
        loop_id: String,
        socket: String,
    },
    /// `loop_list`, most recently updated first.
    Loops {
        loops: Vec<LoopRow>,
    },
    /// `loop_attach`: the journal head at the moment of attaching. With `reset`, the cursor
    /// was from another incarnation (or ahead of the journal) and replay starts at seq 1.
    LoopAttached {
        loop_id: String,
        uid: String,
        head_seq: u64,
        #[serde(default)]
        reset: bool,
    },
    /// `loop_status`.
    LoopSummary {
        summary: Box<LoopSummary>,
    },
    /// `loop_read`: `text` covers bytes `[offset, next_offset)` of a file that is `size`
    /// bytes long right now (lossily decoded at the edges).
    LoopBytes {
        offset: u64,
        next_offset: u64,
        size: u64,
        text: String,
    },
    /// `loop_op` succeeded (failures are `ok:false` with the op error `code`).
    OpResult(OpResult),

    /// A newer peer's response kind this build does not know. Only ever produced by
    /// deserialization: clients ignore it and never act on it.
    #[serde(other)]
    Unknown,
}

/// One row of the daemon's session list (P3 fills `state` with live worker probes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRow {
    pub session_id: String,
    /// "running" | "idle" | "inactive"
    pub state: String,
    pub title: Option<String>,
    pub cwd: String,
    pub updated_at_ms: u64,
}

/// Unsolicited worker→client frames while attached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Streamed output chunk from the in-flight exchange.
    Chunk { text: String },
    /// A turn began (another client, or this one — clients render idempotently).
    TurnStarted {
        backend: String,
        /// Telemetry run id for this turn, so a client can label it (`/outcome`) without
        /// guessing at the newest run in the log. `None` from a daemon predating the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        /// Why the router picked this backend (`profile`, `profile_overall`, `default`, …) —
        /// the route stage is invisible in the TUI otherwise, and an unexplained backend
        /// switch reads as a bug rather than as the learning layer working.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The in-flight turn ended.
    TurnFinished { status: String },
    /// Human-readable side note (recovery marks, detach hints).
    Notice { text: String },

    /// One loop journal record, exactly as written to disk (`rec` is the journal line).
    /// Decode it with `loops::decode_line` after re-serializing; relays pass it through.
    LoopRecord {
        loop_id: String,
        rec: serde_json::Value,
    },
    /// Live output of a running agent step. `offset` is the byte offset of `text` in
    /// `outputs/<step>.log`, so a late or lagging client fills gaps with `loop_read`.
    /// Lossy by design: dropped when the client falls behind.
    LoopChunk {
        loop_id: String,
        step_id: String,
        offset: u64,
        text: String,
    },
    /// Sent every 15 seconds while attached.
    LoopHeartbeat { loop_id: String, head_seq: u64 },

    /// A newer worker's event this build does not know. Only ever produced by
    /// deserialization: attached clients skip it and never act on it.
    #[serde(other)]
    Unknown,
}

/// A single wire frame, for readers that must accept both kinds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Frame {
    Response(Response),
    Event(Event),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_with_flattened_type_tag() {
        let req = Request {
            id: 7,
            body: RequestBody::Send {
                text: "hi".into(),
                backend: Some("codex".into()),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"id\":7"), "{json}");
        assert!(json.contains("\"type\":\"send\""), "{json}");
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);

        // Optional fields stay off the wire.
        let plain = serde_json::to_string(&Request {
            id: 1,
            body: RequestBody::Send {
                text: "x".into(),
                backend: None,
            },
        })
        .unwrap();
        assert!(!plain.contains("backend"), "{plain}");
    }

    #[test]
    fn turn_started_reads_frames_from_a_daemon_without_the_routing_fields() {
        // A running daemon outlives the binary that starts a new client: an installed 0.2.x
        // worker still broadcasts the bare form, and a TUI that fails to parse it renders no
        // turn at all. The fields are additive, never required.
        let old: Event = serde_json::from_str(r#"{"event":"turn_started","backend":"codex"}"#)
            .expect("older frame must still parse");
        assert_eq!(
            old,
            Event::TurnStarted {
                backend: "codex".into(),
                run_id: None,
                reason: None,
            }
        );
        // ... and they stay off the wire when absent, so an older client is unaffected too.
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("run_id"), "{json}");
        assert!(!json.contains("reason"), "{json}");
    }

    #[test]
    fn responses_and_events_disambiguate_via_untagged_frame() {
        let resp = Response::ok(
            3,
            ResponseData::Turn {
                status: "ok".into(),
                answer: "done".into(),
            },
        );
        let event = Event::Chunk {
            text: "partial".into(),
        };
        let resp_line = serde_json::to_string(&resp).unwrap();
        let event_line = serde_json::to_string(&event).unwrap();

        match serde_json::from_str::<Frame>(&resp_line).unwrap() {
            Frame::Response(r) => {
                assert_eq!(r.id, 3);
                assert!(r.ok);
            }
            Frame::Event(_) => panic!("response parsed as event"),
        }
        match serde_json::from_str::<Frame>(&event_line).unwrap() {
            Frame::Event(Event::Chunk { text }) => assert_eq!(text, "partial"),
            other => panic!("event parsed wrong: {other:?}"),
        }
    }

    #[test]
    fn errors_carry_a_message_and_no_data() {
        let resp = Response::err(9, "busy: a turn is already running");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("busy"));
        assert!(!json.contains("\"data\""));
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.error.as_deref(),
            Some("busy: a turn is already running")
        );
    }

    #[test]
    fn hello_pins_the_protocol_version() {
        // A version bump must be deliberate: this test pins v1's wire shape. A bare hello
        // (no features, no client id) must stay byte-identical to what 0.2.x clients send.
        let bare = Request {
            id: 0,
            body: RequestBody::Hello {
                proto: PROTO_VERSION,
                features: vec![],
                client: None,
            },
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert_eq!(json, r#"{"id":0,"type":"hello","proto":1}"#);
        // ... and an older client's bare hello reads back with the additive fields empty.
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back, bare);

        // The additive fields round-trip when present.
        let rich = Request {
            id: 1,
            body: RequestBody::Hello {
                proto: PROTO_VERSION,
                features: vec!["loops/1".into()],
                client: Some("agentpit/9.9.9".into()),
            },
        };
        let json = serde_json::to_string(&rich).unwrap();
        assert_eq!(
            json,
            r#"{"id":1,"type":"hello","proto":1,"features":["loops/1"],"client":"agentpit/9.9.9"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), rich);
    }

    #[test]
    fn hello_response_features_are_additive() {
        let bare = Response::ok(
            0,
            ResponseData::Hello {
                proto: PROTO_VERSION,
                role: "daemon".into(),
                pid: 42,
                features: vec![],
            },
        );
        let json = serde_json::to_string(&bare).unwrap();
        assert_eq!(
            json,
            r#"{"id":0,"ok":true,"data":{"kind":"hello","proto":1,"role":"daemon","pid":42}}"#
        );
        // An older daemon's hello (no `features`) reads back as "offers nothing".
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), bare);

        let rich: Response = serde_json::from_str(
            r#"{"id":0,"ok":true,"data":{"kind":"hello","proto":1,"role":"worker","pid":7,"features":["loops/1"]}}"#,
        )
        .unwrap();
        match rich.data {
            Some(ResponseData::Hello { features, .. }) => assert_eq!(features, ["loops/1"]),
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn ok_and_err_responses_are_byte_identical_without_code_or_details() {
        // Pinned: `code`/`details` are additive and must not appear unless set, so an older
        // client sees exactly the frames it always did.
        assert_eq!(
            serde_json::to_string(&Response::ok(3, ResponseData::Unit)).unwrap(),
            r#"{"id":3,"ok":true,"data":{"kind":"unit"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::err(9, "busy")).unwrap(),
            r#"{"id":9,"ok":false,"error":"busy"}"#
        );
        // An older server's error reads back with no code.
        let old: Response = serde_json::from_str(r#"{"id":9,"ok":false,"error":"busy"}"#).unwrap();
        assert_eq!(old, Response::err(9, "busy"));
        assert_eq!(old.code, None);

        let coded = Response::err_code(4, CODE_UNSUPPORTED, "nope");
        assert_eq!(
            serde_json::to_string(&coded).unwrap(),
            r#"{"id":4,"ok":false,"error":"nope","code":"unsupported"}"#
        );
        let detailed = coded.with_details(serde_json::json!({"type": "teleport"}));
        let json = serde_json::to_string(&detailed).unwrap();
        assert_eq!(
            json,
            r#"{"id":4,"ok":false,"error":"nope","code":"unsupported","details":{"type":"teleport"}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), detailed);
    }

    #[test]
    fn unknown_request_type_parses_through_the_flattened_body() {
        // A newer client's verb must reach the server as `Unknown` (answerable with the
        // caller's id) rather than failing to parse (answerable only with id 0).
        let req: Request = serde_json::from_str(r#"{"id":5,"type":"teleport","to":"x"}"#)
            .expect("unknown request type must parse");
        assert_eq!(
            req,
            Request {
                id: 5,
                body: RequestBody::Unknown,
            }
        );
        // A KNOWN type with bad fields is still a parse error, not `Unknown`.
        assert!(serde_json::from_str::<Request>(r#"{"id":6,"type":"send"}"#).is_err());
        // Unknown fields on a known type are ignored (never `deny_unknown_fields`).
        let req: Request =
            serde_json::from_str(r#"{"id":7,"type":"cancel","reason":"later"}"#).unwrap();
        assert_eq!(req.body, RequestBody::Cancel);
    }

    #[test]
    fn unknown_event_parses_as_frame_event_unknown() {
        let frame: Frame = serde_json::from_str(r#"{"event":"future_thing","x":1}"#)
            .expect("unknown event must parse");
        assert!(
            matches!(frame, Frame::Event(Event::Unknown)),
            "got {frame:?}"
        );
        // A newer field on a known event is ignored, not fatal.
        let frame: Frame =
            serde_json::from_str(r#"{"event":"chunk","text":"hi","seq":3}"#).unwrap();
        assert!(matches!(frame, Frame::Event(Event::Chunk { ref text }) if text == "hi"));
        // A line that is neither a response nor tagged as an event still fails to parse —
        // `Unknown` absorbs only unknown tags, not arbitrary objects.
        assert!(serde_json::from_str::<Frame>(r#"{"hello":"world"}"#).is_err());
    }

    #[test]
    fn unknown_response_kind_parses_as_response_data_unknown() {
        let line = r#"{"id":2,"ok":true,"data":{"kind":"proposal_landed","proposal":"p1"}}"#;
        let resp: Response = serde_json::from_str(line).expect("unknown kind must parse");
        assert_eq!(resp.id, 2);
        assert_eq!(resp.data, Some(ResponseData::Unknown));
        // ... and through the untagged frame, it is still a Response (not an Event).
        match serde_json::from_str::<Frame>(line).unwrap() {
            Frame::Response(r) => assert_eq!(r.data, Some(ResponseData::Unknown)),
            Frame::Event(e) => panic!("response parsed as event: {e:?}"),
        }
    }

    #[test]
    fn loop_ops_are_flat_under_the_request_envelope() {
        // Design §7's example frame, byte for byte.
        let line = r#"{"id":7,"type":"loop_op","loop_id":"lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b","op_id":"0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9d","op":"resolve_gate","gate_id":"g1","option":"approve"}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        let RequestBody::LoopOp(op) = &req.body else {
            panic!("expected loop_op, got {req:?}");
        };
        assert_eq!(op.op.name(), "resolve_gate");
        assert_eq!(op.expect_seq, None);
        let back: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            back,
            serde_json::from_str::<serde_json::Value>(line).unwrap()
        );

        // A newer op still reaches the runner (as `unknown`) with the caller's id.
        let req: Request = serde_json::from_str(
            r#"{"id":8,"type":"loop_op","loop_id":"lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b","op_id":"0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9e","op":"land","proposal":"p1"}"#,
        )
        .unwrap();
        assert!(matches!(req.body, RequestBody::LoopOp(ref o) if o.op.name() == "unknown"));
    }

    #[test]
    fn loop_start_defaults_and_blueprint_sources() {
        let req: Request = serde_json::from_str(
            r#"{"id":1,"type":"loop_start","op_id":"0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9d","blueprint":{"source":"named","name":"fix-until-green"},"cwd":"/w"}"#,
        )
        .unwrap();
        match req.body {
            RequestBody::LoopStart {
                blueprint,
                start,
                inputs,
                origin,
                ..
            } => {
                assert_eq!(
                    blueprint,
                    BlueprintSource::Named {
                        name: "fix-until-green".into()
                    }
                );
                assert!(start, "start defaults to true");
                assert!(inputs.is_empty());
                assert_eq!(origin, None);
            }
            other => panic!("expected loop_start, got {other:?}"),
        }
        let future: BlueprintSource =
            serde_json::from_str(r#"{"source":"registry","id":"x"}"#).unwrap();
        assert_eq!(future, BlueprintSource::Unknown);
        let file: LoopFile = serde_json::from_str(r#""check_log""#).unwrap();
        assert_eq!(file, LoopFile::CheckLog);
        assert_eq!(
            serde_json::from_str::<LoopFile>(r#""diff""#).unwrap(),
            LoopFile::Unknown
        );
    }

    #[test]
    fn loop_frames_round_trip_through_frame() {
        let raw = r#"{"v":1,"seq":23,"ts":1790381211000,"kind":"gate_resolved","op":"0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9d","data":{"gate_id":"g1","option":"approve","by":{"kind":"human","client":"agentpit-dashboard/0.3.0"},"future":1.5}}"#;
        let line = format!(
            r#"{{"event":"loop_record","loop_id":"lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b","rec":{raw}}}"#
        );
        match serde_json::from_str::<Frame>(&line).unwrap() {
            Frame::Event(Event::LoopRecord { rec, .. }) => {
                // The record survives the relay with every field, unknown ones included.
                let decoded =
                    crate::loops::decode_line(&serde_json::to_string(&rec).unwrap()).unwrap();
                assert_eq!(decoded.seq, 23);
                assert_eq!(decoded.kind, "gate_resolved");
                assert_eq!(rec["data"]["future"], 1.5);
            }
            other => panic!("expected loop_record, got {other:?}"),
        }
        let resp = Response::ok(
            7,
            ResponseData::OpResult(OpResult {
                op_id: "0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9d".into(),
                outcome: crate::loops::OpOutcome::Applied,
                seq: Some(23),
                head_seq: 25,
                result: None,
            }),
        );
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(
            json,
            r#"{"id":7,"ok":true,"data":{"kind":"op_result","op_id":"0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9d","outcome":"applied","seq":23,"head_seq":25}}"#
        );
        match serde_json::from_str::<Frame>(&json).unwrap() {
            Frame::Response(r) => assert_eq!(r, resp),
            Frame::Event(e) => panic!("response parsed as event: {e:?}"),
        }
    }

    #[test]
    fn bad_request_echoes_the_line_id() {
        let line = r#"{"id":9,"type":"send"}"#; // `send` without its `text`
        let err = serde_json::from_str::<Request>(line).unwrap_err();
        let resp = Response::bad_request(line, &err);
        assert_eq!(resp.id, 9);
        assert!(!resp.ok);
        assert_eq!(resp.code.as_deref(), Some(CODE_BAD_REQUEST));
        assert!(resp.error.as_deref().unwrap().starts_with("bad request: "));

        // No usable id (not JSON, missing, or not a u64) falls back to 0.
        for line in ["not json", r#"{"type":"send"}"#, r#"{"id":"9","type":"x"}"#] {
            assert_eq!(Response::bad_request(line, "e").id, 0, "{line}");
        }
    }
}
