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

use serde::{Deserialize, Serialize};

/// Bumped on breaking wire changes; checked in the `hello` handshake by BOTH sides.
pub const PROTO_VERSION: u32 = 1;

/// [`Response::code`] for a line that did not parse as a [`Request`].
pub const CODE_BAD_REQUEST: &str = "bad_request";
/// [`Response::code`] for a request type the server does not know (a newer peer's verb).
pub const CODE_UNSUPPORTED: &str = "unsupported";

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

    /// A newer peer's request type this build does not know. Only ever produced by
    /// deserialization: servers answer it with a [`CODE_UNSUPPORTED`] error and never act
    /// on it. Its fields are dropped, so it is not meant to be sent.
    #[serde(other)]
    Unknown,
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
        let line = r#"{"id":2,"ok":true,"data":{"kind":"loop_started","loop_id":"l1"}}"#;
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
