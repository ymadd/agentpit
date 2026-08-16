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

use serde::{Deserialize, Serialize};

/// Bumped on breaking wire changes; checked in the `hello` handshake by BOTH sides.
pub const PROTO_VERSION: u32 = 1;

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
    Hello { proto: u32 },

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
}

/// A response frame: `ok:true` carries `data`, `ok:false` carries `error`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ResponseData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: u64, data: ResponseData) -> Self {
        Response {
            id,
            ok: true,
            data: Some(data),
            error: None,
        }
    }
    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Response {
            id,
            ok: false,
            data: None,
            error: Some(error.into()),
        }
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
        // A version bump must be deliberate: this test pins v1's wire shape.
        let json = serde_json::to_string(&Request {
            id: 0,
            body: RequestBody::Hello {
                proto: PROTO_VERSION,
            },
        })
        .unwrap();
        assert_eq!(json, r#"{"id":0,"type":"hello","proto":1}"#);
    }
}
