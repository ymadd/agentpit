//! Workspace loops: blueprints executed as bounded loops (docs/workspace-loop-design.md).
//!
//! A **blueprint** is a human-readable, editable graph (agent / check / gate / repeat
//! nodes). A **loop** is one execution of one frozen blueprint revision. Every fact about a
//! loop is one line in its append-only journal (`loops/<loop_id>/journal.jsonl`), and the
//! loop's state is a pure fold of those lines — so the runner that writes the journal owns
//! nothing a restart cannot rebuild (design §14.3), and the dashboard bridge folds with the
//! exact same code (design §11).
//!
//! This module is the shared schema (design §16): the dashboard depends only on
//! agentpit-events, so every type both sides need lives here. Everything is pure data and
//! pure functions except [`journal`], which owns the on-disk format.
//!
//! Compatibility rules (design §13) in one paragraph: every string enum ends in `Unknown`
//! (a newer writer's value) which is rendered but never acted on; unknown record kinds are
//! kept, and a *critical* one (or a state-driving `Unknown` value) makes the journal
//! read-only for this build; fields are only ever added, as optional.

pub mod blueprint;
pub mod journal;
pub mod ops;
pub mod record;
pub mod state;
pub mod store;
pub mod view;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use blueprint::*;
pub use journal::*;
pub use ops::*;
pub use record::*;
pub use state::*;
pub use store::*;
pub use view::*;

/// Bumped by additive schema changes (new kinds, new optional fields, new enum values).
/// Written into every `writer_opened`; a journal last opened by a writer with a HIGHER
/// minor is read-only for this build, because that writer may have recorded fields this
/// build would silently drop (design §13 C2).
pub const SCHEMA_MINOR: u16 = 0;

/// One journal line, including the trailing `'\n'`.
pub const MAX_LINE_BYTES: usize = 256 * 1024;
/// Canonical JSON of one blueprint document.
pub const MAX_DOC_BYTES: usize = 128 * 1024;
/// Agent task, gate prompt, instruction text.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Feedback detail, log tail, error text, gate comment, warning message.
pub const MAX_DETAIL_BYTES: usize = 4 * 1024;
/// Titles, feedback summaries, excerpts.
pub const MAX_SHORT_BYTES: usize = 512;
/// Options on one gate.
pub const MAX_GATE_OPTIONS: usize = 8;
/// Feedback items carried by one record: bounds `feedback: all` over many iterations.
pub const MAX_FEEDBACK_ITEMS: usize = 16;
/// `LoopSummary.open_gates` shows at most this many (the count is exact).
pub const SUMMARY_OPEN_GATES: usize = 5;
/// Gate prompts are clamped to this many bytes inside a `LoopSummary`.
pub const SUMMARY_PROMPT_BYTES: usize = 280;
/// `LoopState.warnings` keeps the newest this many.
pub const MAX_WARNINGS: usize = 200;

/// `<state>/loops/`: one directory per loop.
pub const LOOPS_DIR: &str = "loops";
/// `<state>/loop-leases/`: single-writer leases, one per loop directory.
pub const LOOP_LEASES_DIR: &str = "loop-leases";
/// The journal inside a loop directory.
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// Declares a wire string enum: snake_case spellings pinned per variant, a trailing
/// `Unknown` catch-all for values written by a newer agentpit, and `as_str` / `KNOWN`
/// for tests and display. Writers never serialize `Unknown`.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $wire:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name {
            $( $(#[$vmeta])* #[serde(rename = $wire)] $variant, )+
            /// A value this build does not know (written by a newer agentpit).
            #[serde(rename = "unknown", other)]
            Unknown,
        }

        impl $name {
            /// Every known variant, in declaration order.
            pub const KNOWN: &'static [$name] = &[ $( $name::$variant ),+ ];

            pub fn as_str(&self) -> &'static str {
                match self {
                    $( $name::$variant => $wire, )+
                    $name::Unknown => "unknown",
                }
            }

            pub fn is_unknown(&self) -> bool {
                matches!(self, $name::Unknown)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}
pub(crate) use string_enum;

string_enum! {
    /// How a step (or a node instance) ended. `cancelled` is never routable by an edge.
    pub enum Outcome {
        Ok = "ok",
        Fail = "fail",
        Error = "error",
        Timeout = "timeout",
        Cancelled = "cancelled",
        /// A repeat that used every iteration without a clean pass.
        Exhausted = "exhausted",
    }
}

string_enum! {
    /// Blueprint node kinds this build can run.
    pub enum NodeKind {
        Agent = "agent",
        Check = "check",
        Gate = "gate",
        Repeat = "repeat",
    }
}

impl NodeKind {
    /// Agent and check steps consume compute, budget steps, and concurrency slots; gate
    /// and repeat steps only wait.
    pub fn is_compute(&self) -> bool {
        matches!(self, NodeKind::Agent | NodeKind::Check)
    }
}

string_enum! {
    /// Whether a compute step may modify the working tree.
    #[derive(Default)]
    pub enum Access {
        #[default]
        Write = "write",
        Read = "read",
    }
}

string_enum! {
    /// Where compute steps run. `in_place` edits the user's tree (v1); `worktree` is the
    /// isolated mode that produces diff proposals (phase 4).
    #[derive(Default)]
    pub enum WorkspaceMode {
        #[default]
        InPlace = "in_place",
        Worktree = "worktree",
    }
}

string_enum! {
    pub enum ActorKind {
        Human = "human",
        System = "system",
        /// A gate deadline resolved the gate.
        Timeout = "timeout",
        Agent = "agent",
    }
}

/// Who caused a record: a person through a client, the runner itself, or a deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub kind: ActorKind,
    /// `"agentpit-dashboard/0.3.0"`, `"cli"`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

impl Actor {
    pub fn human(client: &str) -> Self {
        Actor {
            kind: ActorKind::Human,
            client: Some(client.to_string()),
        }
    }

    pub fn system() -> Self {
        Actor {
            kind: ActorKind::System,
            client: None,
        }
    }
}

/// "Which agent" a step went to. Plain strings, never `BackendId`: a backend added by a
/// newer build must not make the whole line unparseable here (design §3, §13 C6).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Assignee {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// `"exec"` or `"acp"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// `"role"`, `"explicit"`, or the router's reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

/// A file inside the loop directory holding a payload too large for a journal line
/// (rendered prompts, full outputs). Written and fsynced before the record naming it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// Relative to the loop directory; always passes [`is_safe_rel_path`].
    pub path: String,
    pub bytes: u64,
}

/// A reviewer agent's verdict (`VERDICT: PASS|FAIL` on its last line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub pass: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<String>,
}

string_enum! {
    pub enum FeedbackSource {
        /// A failed check's output tail.
        Check = "check",
        /// A reviewer agent's FAIL findings.
        Verdict = "verdict",
        /// A person's gate comment (later: rejected hunks + comments).
        Human = "human",
        /// An agent/check error.
        Error = "error",
    }
}

/// What the next iteration is told about the previous one — the fix for cascade's
/// "every hop gets the same task" (design §4.3 rule 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackItem {
    pub source: FeedbackSource,
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// At most [`MAX_SHORT_BYTES`].
    pub summary: String,
    /// At most [`MAX_DETAIL_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Hard limits on one loop. Exceeding one never fails silently: the runner opens a budget
/// gate (or fails the loop when the blueprint says `on_budget: fail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Agent and check attempts (automatic retries and human retries included).
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// Wall-clock seconds during which at least one compute step runs.
    #[serde(default = "default_max_active_secs")]
    pub max_active_secs: u64,
    /// Compute steps running at once.
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
}

pub const DEFAULT_MAX_STEPS: u32 = 40;
pub const DEFAULT_MAX_ACTIVE_SECS: u64 = 4 * 60 * 60;
pub const DEFAULT_MAX_PARALLEL: u32 = 1;
pub const HARD_MAX_STEPS: u32 = 500;
pub const HARD_MAX_PARALLEL: u32 = 4;
pub const MIN_ACTIVE_SECS: u64 = 60;
pub const HARD_MAX_ACTIVE_SECS: u64 = 7 * 24 * 60 * 60;

fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}
fn default_max_active_secs() -> u64 {
    DEFAULT_MAX_ACTIVE_SECS
}
fn default_max_parallel() -> u32 {
    DEFAULT_MAX_PARALLEL
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_steps: DEFAULT_MAX_STEPS,
            max_active_secs: DEFAULT_MAX_ACTIVE_SECS,
            max_parallel: DEFAULT_MAX_PARALLEL,
        }
    }
}

impl Budget {
    /// `None` when every field is inside the hard caps; otherwise the offending field.
    pub fn out_of_range(&self) -> Option<&'static str> {
        if !(1..=HARD_MAX_STEPS).contains(&self.max_steps) {
            return Some("max_steps");
        }
        if !(MIN_ACTIVE_SECS..=HARD_MAX_ACTIVE_SECS).contains(&self.max_active_secs) {
            return Some("max_active_secs");
        }
        if !(1..=HARD_MAX_PARALLEL).contains(&self.max_parallel) {
            return Some("max_parallel");
        }
        None
    }
}

/// What a loop has consumed so far (a fold result, never written).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub steps: u32,
    pub active_ms: u64,
}

// ---------------------------------------------------------------------------------------
// Ids. Opaque to clients; the formats exist for determinism and debugging, and every
// generated id passes `is_safe_log_component` so it can name a file.

/// `lp-` + 32 lowercase hex digits.
pub fn is_valid_loop_id(s: &str) -> bool {
    s.len() == 35
        && s.starts_with("lp-")
        && s[3..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The loop id a client's `loop_start` op id names, so a retried start is idempotent.
/// Only the canonical lowercase hyphenated UUID form is accepted — other spellings of the
/// same UUID would be distinct op ids mapping to one loop — and the nil UUID is refused.
pub fn loop_id_from_op(op_id: &str) -> Option<String> {
    let canonical = op_id.len() == 36
        && op_id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        });
    if !canonical {
        return None;
    }
    let hex: String = op_id.chars().filter(|c| *c != '-').collect();
    if hex.bytes().all(|b| b == b'0') {
        return None;
    }
    Some(format!("lp-{hex}"))
}

/// A fresh operation id: a UUIDv7 (time-sortable, canonical form, so it may also name a
/// loop via [`loop_id_from_op`]).
pub fn new_op_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Client-chosen idempotency key: 8..=64 bytes of `[A-Za-z0-9._-]`. UUIDv7 recommended.
pub fn is_valid_op_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && crate::is_safe_log_component(s)
}

/// `^[a-z][a-z0-9_-]{0,31}$`: no dots, because `.` separates step-id parts.
pub fn is_valid_node_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// `^[a-z0-9][a-z0-9_-]{0,63}$`: the same family as role and workflow-type names.
pub fn is_valid_blueprint_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// `^[a-z][a-z0-9_]{0,31}$`.
pub fn is_valid_input_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// `^[a-z0-9_-]{1,32}$`.
pub fn is_valid_option_id(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

/// A node instance ("段階"): the node plus the iteration numbers of its enclosing repeats,
/// outer to inner. `implement.i1-2` = node `implement`, outer iteration 1, inner 2.
pub fn instance_key(node: &str, iter: &[u32]) -> String {
    if iter.is_empty() {
        return node.to_string();
    }
    let path: Vec<String> = iter.iter().map(u32::to_string).collect();
    format!("{node}.i{}", path.join("-"))
}

/// One attempt at a node instance: `plan.a1`, `implement.i1-2.a3`.
pub fn step_id(node: &str, iter: &[u32], attempt: u32) -> String {
    format!("{}.a{attempt}", instance_key(node, iter))
}

/// Gates are numbered per loop from 1: `g1`, `g2`, …
pub fn gate_id(n: usize) -> String {
    format!("g{n}")
}

/// Instructions are numbered per loop from 1: `in1`, `in2`, …
pub fn instruction_id(n: usize) -> String {
    format!("in{n}")
}

/// 16 random hex digits, written into `loop_created`. Detects a loop directory that was
/// deleted and recreated under the same loop id, so a client cursor from the old
/// incarnation resets instead of silently resuming onto different history.
pub fn new_uid() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// A relative path that stays inside its base: non-empty, not absolute, no `\0` or `\\`,
/// and no empty, `.` or `..` component.
pub fn is_safe_rel_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 4096
        && !p.starts_with('/')
        && !p.contains('\0')
        && !p.contains('\\')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// `text` truncated to at most `max` bytes on a `char` boundary.
pub fn clamp_text(text: &str, max: usize) -> String {
    crate::truncate_on_char_boundary(text, max).to_string()
}

// ---------------------------------------------------------------------------------------
// Paths (validate before join).

/// `<state>/loops`.
pub fn loops_dir() -> PathBuf {
    crate::state_dir().join(LOOPS_DIR)
}

/// `<state>/loop-leases`.
pub fn loop_leases_dir() -> PathBuf {
    crate::state_dir().join(LOOP_LEASES_DIR)
}

/// `<loops_root>/<loop_id>`, or `None` for an id that is not a valid loop id.
pub fn loop_dir_in(loops_root: &Path, loop_id: &str) -> Option<PathBuf> {
    is_valid_loop_id(loop_id).then(|| loops_root.join(loop_id))
}

/// `<state>/loops/<loop_id>`, or `None` for an invalid id.
pub fn loop_dir(loop_id: &str) -> Option<PathBuf> {
    loop_dir_in(&loops_dir(), loop_id)
}

/// `<loop_dir>/journal.jsonl`.
pub fn journal_path(loop_dir: &Path) -> PathBuf {
    loop_dir.join(JOURNAL_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_id_is_derived_from_the_canonical_op_uuid_only() {
        assert_eq!(
            loop_id_from_op("0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").as_deref(),
            Some("lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b")
        );
        // Other spellings of the same UUID would be distinct op ids for one loop.
        assert_eq!(
            loop_id_from_op("0199A1B2-C3D4-7E5F-8A9B-0C1D2E3F4A5B"),
            None
        );
        assert_eq!(loop_id_from_op("0199a1b2c3d47e5f8a9b0c1d2e3f4a5b"), None);
        assert_eq!(
            loop_id_from_op("00000000-0000-0000-0000-000000000000"),
            None
        );
        assert_eq!(loop_id_from_op("not-a-uuid"), None);
        let derived = loop_id_from_op("0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").unwrap();
        assert!(is_valid_loop_id(&derived));
        assert!(crate::is_safe_log_component(&derived));
    }

    #[test]
    fn node_ids_reject_dots_uppercase_and_leading_digits() {
        assert!(is_valid_node_id("implement"));
        assert!(is_valid_node_id("fix_2-b"));
        assert!(!is_valid_node_id("a.b"));
        assert!(!is_valid_node_id("Plan"));
        assert!(!is_valid_node_id("2plan"));
        assert!(!is_valid_node_id("@break"));
        assert!(!is_valid_node_id(""));
        assert!(!is_valid_node_id(&"a".repeat(33)));
    }

    #[test]
    fn step_ids_encode_node_iteration_path_and_attempt() {
        assert_eq!(step_id("plan", &[], 1), "plan.a1");
        assert_eq!(step_id("implement", &[1, 2], 3), "implement.i1-2.a3");
        assert_eq!(instance_key("test", &[4]), "test.i4");
        assert!(crate::is_safe_log_component(&step_id(
            "implement",
            &[1, 2],
            3
        )));
        assert_eq!(gate_id(1), "g1");
        assert_eq!(instruction_id(2), "in2");
    }

    #[test]
    fn loop_dir_rejects_unsafe_ids() {
        let root = Path::new("/tmp/x");
        assert!(loop_dir_in(root, "../etc").is_none());
        assert!(loop_dir_in(root, "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5").is_none());
        assert_eq!(
            loop_dir_in(root, "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b").unwrap(),
            root.join("lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b")
        );
    }

    #[test]
    fn safe_rel_paths_stay_inside_their_base() {
        assert!(is_safe_rel_path("prompts/plan.a1.md"));
        assert!(!is_safe_rel_path("/etc/passwd"));
        assert!(!is_safe_rel_path("a/../b"));
        assert!(!is_safe_rel_path("a//b"));
        assert!(!is_safe_rel_path("./a"));
        assert!(!is_safe_rel_path("a\\b"));
        assert!(!is_safe_rel_path(""));
    }

    #[test]
    fn budget_serde_default_equals_default_impl() {
        let parsed: Budget = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, Budget::default());
        assert_eq!(Budget::default().out_of_range(), None);
        let zero = Budget {
            max_parallel: 0,
            ..Budget::default()
        };
        assert_eq!(zero.out_of_range(), Some("max_parallel"));
    }

    #[test]
    fn string_enums_serialize_as_their_as_str_and_unknown_values_parse() {
        for o in Outcome::KNOWN {
            assert_eq!(
                serde_json::to_value(o).unwrap(),
                serde_json::Value::from(o.as_str())
            );
            let back: Outcome = serde_json::from_value(o.as_str().into()).unwrap();
            assert_eq!(&back, o);
        }
        for m in WorkspaceMode::KNOWN {
            assert_eq!(
                serde_json::to_value(m).unwrap(),
                serde_json::Value::from(m.as_str())
            );
        }
        let future: Outcome = serde_json::from_str("\"partial\"").unwrap();
        assert!(future.is_unknown());
        assert_eq!(future.as_str(), "unknown");
    }

    #[test]
    fn uids_are_16_hex_digits() {
        let uid = new_uid();
        assert_eq!(uid.len(), 16);
        assert!(uid.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(uid, new_uid());
    }
}
