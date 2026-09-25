//! Blueprints: the human-readable, editable loop graph (design §4).
//!
//! A blueprint is a JSON document (`.agentpit/blueprints/<name>.json` in a project, or
//! `~/.config/agentpit/blueprints/`). The raw `serde_json::Value` is authoritative: the
//! typed [`Blueprint`] is a view parsed from it, and callers save the raw document they
//! validated — never a re-serialized view — so keys this build does not know survive an
//! edit by it. A running loop never re-reads the file; the document is frozen into
//! `loop_created`.
//!
//! The language is deliberately small (design §4.2): four node kinds, edges that route on
//! a closed set of outcomes, and structured `repeat` nodes as the only way to loop. Every
//! scope is a DAG, every repeat has `max_iterations`, so [`Bounds::worst_steps`] is a
//! finite, statically known ceiling on compute steps — the termination argument behind
//! the principle revision (design §14.1).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    is_safe_rel_path, is_valid_blueprint_name, is_valid_input_name, is_valid_node_id,
    is_valid_option_id, string_enum, Access, Budget, NodeKind, Outcome, WorkspaceMode,
    MAX_DOC_BYTES, MAX_GATE_OPTIONS, MAX_SHORT_BYTES, MAX_TEXT_BYTES,
};

pub const BLUEPRINT_SCHEMA_V1: &str = "agentpit.blueprint/1";
pub const MAX_NODES: usize = 64;
pub const MAX_REPEAT_ITERATIONS: u32 = 20;
/// Repeat ancestors a node may have.
pub const MAX_REPEAT_NESTING: usize = 3;
/// Automatic retries of one agent/check instance on error or timeout.
pub const MAX_RETRIES: u32 = 3;
pub const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// Optional capabilities a document may declare in `requires`. A newer blueprint whose
/// meaning depends on keys this build would silently ignore must list a feature here, so
/// an older daemon refuses it instead of running it with those constraints dropped.
/// v1 defines none.
pub const SUPPORTED_REQUIRES: &[&str] = &[];

/// The typed view of a blueprint document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Blueprint {
    pub schema: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, InputSpec>,
    #[serde(default)]
    pub workspace: WorkspaceSpec,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub policy: Policy,
    pub nodes: Vec<Node>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<Edge>,
    /// UI geometry (React Flow positions, group sizes). The engine ignores it and it is
    /// excluded from the revision, so dragging a node never changes the rev.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InputSpec {
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkspaceSpec {
    /// Absent means `in_place`, forever (changing a default is a breaking change).
    #[serde(default)]
    pub mode: WorkspaceMode,
}

string_enum! {
    /// What happens when an agent/check still errors (or times out) after its retries.
    pub enum OnError {
        /// Open a `step_error` gate: retry / treat as failed / stop.
        Gate = "gate",
        /// Treat it as the node's outcome and route on it.
        Fail = "fail",
    }
}

string_enum! {
    /// What happens when the budget runs out.
    pub enum OnBudget {
        /// Open a `budget` gate: extend / stop.
        Gate = "gate",
        /// Fail the loop.
        Fail = "fail",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default = "default_on_error")]
    pub on_error: OnError,
    #[serde(default = "default_on_budget")]
    pub on_budget: OnBudget,
}

fn default_on_error() -> OnError {
    OnError::Gate
}
fn default_on_budget() -> OnBudget {
    OnBudget::Gate
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            on_error: OnError::Gate,
            on_budget: OnBudget::Gate,
        }
    }
}

/// One node. Nodes form a flat array; `parent` names the enclosing `repeat` (React Flow's
/// `parentId` maps onto it directly).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(flatten)]
    pub spec: NodeSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeSpec {
    Agent(AgentSpec),
    Check(CheckSpec),
    Gate(GateSpec),
    Repeat(RepeatSpec),
    /// A kind this build does not run (`manager`, `ensemble`, `arena`, …, or one from a
    /// newer agentpit). It parses, so the document stays editable, but never validates as
    /// runnable.
    #[serde(other)]
    Unsupported,
}

impl NodeSpec {
    pub fn kind(&self) -> NodeKind {
        match self {
            NodeSpec::Agent(_) => NodeKind::Agent,
            NodeSpec::Check(_) => NodeKind::Check,
            NodeSpec::Gate(_) => NodeKind::Gate,
            NodeSpec::Repeat(_) => NodeKind::Repeat,
            NodeSpec::Unsupported => NodeKind::Unknown,
        }
    }
}

/// One stateless dispatch (design §4.2). The cast is a `role` (resolved through
/// `[workflow.roles]`), an explicit `backend`, or — with neither — the router, optionally
/// hinted with a `category`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Prompt template (see [`placeholders`]).
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default)]
    pub access: Access,
    /// Read a trailing `VERDICT: PASS|FAIL` line: FAIL makes the outcome `fail` and its
    /// findings become feedback for the next iteration.
    #[serde(default)]
    pub verdict: bool,
    /// Automatic retries on error/timeout (0..=3).
    #[serde(default)]
    pub retries: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// A deterministic predicate: `sh -c <command>`, exit 0 = ok. The command is NOT a
/// template, so agent output can never be spliced into a shell line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    pub command: String,
    /// Working directory relative to the loop's cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub retries: u32,
}

/// A human decision point. Each option maps to an outcome (`ok` or `fail`) for routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateSpec {
    /// Prompt template (see [`placeholders`]); `{{instructions}}` is not available here.
    pub prompt: String,
    /// Empty means [`default_gate_options`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<GateOptionSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Option chosen at the deadline; absent means the gate step ends with `timeout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_timeout: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateOptionSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub outcome: Outcome,
}

/// `[approve → ok, reject → fail]`.
pub fn default_gate_options() -> Vec<GateOptionSpec> {
    vec![
        GateOptionSpec {
            id: "approve".into(),
            label: Some("Approve".into()),
            outcome: Outcome::Ok,
        },
        GateOptionSpec {
            id: "reject".into(),
            label: Some("Reject".into()),
            outcome: Outcome::Fail,
        },
    ]
}

/// A bounded loop over its children (the nodes whose `parent` is this node).
///
/// Each iteration runs the children as a fresh scope. When the scope settles the
/// iteration is decided: **break** if no child ended with an outcome that no edge handles,
/// otherwise **continue** — or **exhausted** on the last iteration. "Repeat until the body
/// passes cleanly" (design §4.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepeatSpec {
    pub max_iterations: u32,
    #[serde(default)]
    pub feedback: FeedbackMode,
}

string_enum! {
    /// Which feedback the next iteration's prompts receive.
    #[derive(Default)]
    pub enum FeedbackMode {
        /// The previous iteration's.
        #[default]
        Last = "last",
        /// Every earlier iteration's (bounded by `MAX_FEEDBACK_ITEMS`).
        All = "all",
        None = "none",
    }
}

/// A control-flow edge between two nodes of the same scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    #[serde(default = "default_edge_on")]
    pub on: EdgeOn,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn default_edge_on() -> EdgeOn {
    EdgeOn::Ok
}

string_enum! {
    /// Which outcomes of `from` fire the edge.
    pub enum EdgeOn {
        Ok = "ok",
        Fail = "fail",
        Error = "error",
        Timeout = "timeout",
        Exhausted = "exhausted",
        /// fail, error, timeout or exhausted.
        NotOk = "not_ok",
        /// Any outcome except `cancelled`.
        Always = "always",
    }
}

impl EdgeOn {
    pub fn matches(&self, outcome: Outcome) -> bool {
        match self {
            EdgeOn::Ok => outcome == Outcome::Ok,
            EdgeOn::Fail => outcome == Outcome::Fail,
            EdgeOn::Error => outcome == Outcome::Error,
            EdgeOn::Timeout => outcome == Outcome::Timeout,
            EdgeOn::Exhausted => outcome == Outcome::Exhausted,
            EdgeOn::NotOk => matches!(
                outcome,
                Outcome::Fail | Outcome::Error | Outcome::Timeout | Outcome::Exhausted
            ),
            EdgeOn::Always => !matches!(outcome, Outcome::Cancelled | Outcome::Unknown),
            EdgeOn::Unknown => false,
        }
    }

    /// The `on` values an edge out of a node of `kind` may use.
    pub fn allowed_for(kind: NodeKind) -> &'static [EdgeOn] {
        use EdgeOn::*;
        match kind {
            NodeKind::Agent | NodeKind::Check => &[Ok, Fail, Error, Timeout, NotOk, Always],
            NodeKind::Gate => &[Ok, Fail, Timeout, NotOk, Always],
            NodeKind::Repeat => &[Ok, Exhausted, NotOk, Always],
            NodeKind::Unknown => &[],
        }
    }
}

// ---------------------------------------------------------------------------------------
// Structure.

/// Structural lookups shared by validation and the loop fold. Built from any typed
/// blueprint — including invalid ones (duplicate ids, unknown parents, parent cycles) —
/// and every walk is bounded, so it never loops.
#[derive(Debug, Clone, Default)]
pub struct BlueprintIndex {
    by_id: BTreeMap<String, usize>,
    parent: Vec<Option<String>>,
    ids: Vec<String>,
}

impl Blueprint {
    pub fn index(&self) -> BlueprintIndex {
        let mut by_id = BTreeMap::new();
        for (i, n) in self.nodes.iter().enumerate() {
            by_id.entry(n.id.clone()).or_insert(i);
        }
        BlueprintIndex {
            by_id,
            parent: self.nodes.iter().map(|n| n.parent.clone()).collect(),
            ids: self.nodes.iter().map(|n| n.id.clone()).collect(),
        }
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Capabilities a daemon must advertise to run this document: `bp.node.<kind>` for
    /// every kind present, `bp.workspace.<mode>`, and every declared `requires` entry.
    pub fn required_features(&self) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = self
            .nodes
            .iter()
            .map(|n| format!("bp.node.{}", n.spec.kind().as_str()))
            .collect();
        out.insert(format!("bp.workspace.{}", self.workspace.mode.as_str()));
        out.extend(self.requires.iter().cloned());
        out
    }

    /// True when every node kind and every state-driving enum is known to this build.
    /// Unlike [`validate`] this never tightens across versions, so it is what decides
    /// whether a frozen blueprint may keep running after an upgrade (design §13).
    pub fn is_understood(&self) -> bool {
        self.schema == BLUEPRINT_SCHEMA_V1
            && self.workspace.mode != WorkspaceMode::Unknown
            && !self.policy.on_error.is_unknown()
            && !self.policy.on_budget.is_unknown()
            && self
                .requires
                .iter()
                .all(|r| SUPPORTED_REQUIRES.contains(&r.as_str()))
            && self.edges.iter().all(|e| !e.on.is_unknown())
            && self.nodes.iter().all(|n| match &n.spec {
                NodeSpec::Unsupported => false,
                NodeSpec::Agent(a) => !a.access.is_unknown(),
                NodeSpec::Check(_) => true,
                NodeSpec::Gate(g) => g.options.iter().all(|o| !o.outcome.is_unknown()),
                NodeSpec::Repeat(r) => !r.feedback.is_unknown(),
            })
    }
}

impl BlueprintIndex {
    pub fn contains(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    fn parent_of(&self, id: &str) -> Option<&str> {
        self.by_id.get(id).and_then(|&i| self.parent[i].as_deref())
    }

    /// The enclosing repeat ids of `id`, OUTER → INNER, excluding `id` itself. Stops at an
    /// unknown parent or a parent cycle.
    pub fn repeat_chain(&self, id: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = id;
        for _ in 0..=self.ids.len() {
            match self.parent_of(cur) {
                Some(p) if self.contains(p) && p != id && !chain.iter().any(|c| c == p) => {
                    chain.push(p.to_string());
                    cur = p;
                }
                _ => break,
            }
        }
        chain.reverse();
        chain
    }

    /// Whether `id` sits (transitively) inside `ancestor`.
    pub fn is_descendant(&self, id: &str, ancestor: &str) -> bool {
        self.repeat_chain(id).iter().any(|r| r == ancestor)
    }

    /// Direct children of `repeat`, in declaration order.
    pub fn children(&self, repeat: &str) -> Vec<String> {
        self.ids
            .iter()
            .zip(&self.parent)
            .filter(|(_, p)| p.as_deref() == Some(repeat))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Nodes with no parent, in declaration order.
    pub fn top_level(&self) -> Vec<String> {
        self.ids
            .iter()
            .zip(&self.parent)
            .filter(|(_, p)| p.is_none())
            .map(|(id, _)| id.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// Revision.

/// Deterministic JSON: object keys sorted by byte order, arrays in order, no whitespace.
/// Sorting is explicit so serde_json's `preserve_order` feature (which feature
/// unification may switch on) can never change a revision.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                write_canonical(&map[k.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&serde_json::to_string(scalar).unwrap_or_default()),
    }
}

/// `b1-<fnv1a64 of the canonical document without its top-level "layout">`. An etag for
/// optimistic concurrency and "which revision is this loop running", not a security hash.
pub fn blueprint_rev(doc: &Value) -> String {
    let body = match doc {
        Value::Object(map) if map.contains_key("layout") => {
            let mut map = map.clone();
            map.remove("layout");
            canonical_json(&Value::Object(map))
        }
        other => canonical_json(other),
    };
    format!("b1-{}", crate::fnv1a_64_hex(body.as_bytes()))
}

// ---------------------------------------------------------------------------------------
// Templates.

/// The placeholders of a template, trimmed, in order. `Err(byte offset)` for a `{{` with
/// no closing `}}`. Valid expressions (checked by [`validate`]): `goal`, `inputs.<name>`,
/// `iteration`, `feedback`, `instructions`, `nodes.<id>.output`, `nodes.<id>.outcome`.
/// There are no conditionals, filters or escapes; rendering happens in the runner.
pub fn placeholders(template: &str) -> Result<Vec<String>, usize> {
    let mut out = Vec::new();
    let mut rest = template;
    let mut offset = 0;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(offset + start);
        };
        out.push(after[..end].trim().to_string());
        let consumed = start + 2 + end + 2;
        offset += consumed;
        rest = &rest[consumed..];
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Validation.

/// Optional knowledge of the local config, used only for warnings.
#[derive(Debug, Clone, Default)]
pub struct ValidateEnv {
    pub roles: Option<BTreeSet<String>>,
    pub backends: Option<BTreeSet<String>>,
}

string_enum! {
    pub enum Severity {
        Error = "error",
        Warning = "warning",
    }
}

/// One finding. `code` is a stable snake_case identifier; `message` is one English
/// sentence ending with the fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    /// `"nodes[3].max_iterations"`, `"edges[2].to"`, `"budget.max_steps"`, or `""`.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub message: String,
}

/// Static ceilings computed from a structurally valid blueprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    /// Maximum agent/check attempts the graph can make without human-requested retries:
    /// `Σ agent|check (1 + retries)`, `gate = 0`, `repeat = max_iterations × body`.
    pub worst_steps: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Validation {
    pub rev: String,
    /// Present whenever the typed parse succeeded, even alongside other errors.
    pub blueprint: Option<Blueprint>,
    pub diagnostics: Vec<Diagnostic>,
    /// Present only when the structure (ids, parents, edges, cycles) is sound.
    pub bounds: Option<Bounds>,
}

impl Validation {
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// Whether the document may be started.
    pub fn is_runnable(&self) -> bool {
        self.blueprint.is_some() && !self.has_errors()
    }

    pub fn codes(&self) -> Vec<&str> {
        self.diagnostics.iter().map(|d| d.code.as_str()).collect()
    }
}

#[derive(Default)]
struct Diags {
    list: Vec<Diagnostic>,
    structural: bool,
}

impl Diags {
    fn push(&mut self, sev: Severity, code: &str, path: String, node: Option<&str>, msg: String) {
        self.list.push(Diagnostic {
            severity: sev,
            code: code.to_string(),
            path,
            node: node.map(str::to_string),
            message: msg,
        });
    }

    fn err(&mut self, code: &str, path: impl Into<String>, node: Option<&str>, msg: String) {
        self.push(Severity::Error, code, path.into(), node, msg);
    }

    /// A structural error: bounds and scope analysis would be meaningless after it.
    fn structural(&mut self, code: &str, path: impl Into<String>, node: Option<&str>, msg: String) {
        self.structural = true;
        self.err(code, path, node, msg);
    }

    fn warn(&mut self, code: &str, path: impl Into<String>, node: Option<&str>, msg: String) {
        self.push(Severity::Warning, code, path.into(), node, msg);
    }
}

/// Validate a blueprint document. Pure; shared by the daemon (start, save), the
/// dashboard (live editing) and the CLI (`agentpit blueprint validate`).
pub fn validate(doc: &Value, env: &ValidateEnv) -> Validation {
    let rev = blueprint_rev(doc);
    let mut d = Diags::default();
    let done = |d: Diags, blueprint: Option<Blueprint>, bounds: Option<Bounds>| Validation {
        rev: rev.clone(),
        blueprint,
        diagnostics: d.list,
        bounds,
    };

    let Some(obj) = doc.as_object() else {
        d.err(
            "invalid_shape",
            "",
            None,
            "a blueprint must be a JSON object; start from an example blueprint.".into(),
        );
        return done(d, None, None);
    };
    match obj.get("schema").and_then(Value::as_str) {
        Some(BLUEPRINT_SCHEMA_V1) => {}
        Some(s) if s.starts_with("agentpit.blueprint/") => {
            d.err(
                "schema_unsupported",
                "schema",
                None,
                format!("schema {s:?} requires a newer agentpit; upgrade agentpit to run it."),
            );
            return done(d, None, None);
        }
        _ => {
            d.err(
                "schema_unsupported",
                "schema",
                None,
                format!("\"schema\" must be {BLUEPRINT_SCHEMA_V1:?}; set it at the top of the document."),
            );
            return done(d, None, None);
        }
    }
    let bp: Blueprint = match serde_json::from_value(doc.clone()) {
        Ok(bp) => bp,
        Err(e) => {
            d.err(
                "invalid_shape",
                "",
                None,
                format!("the document does not match the blueprint shape ({e}); fix that field."),
            );
            return done(d, None, None);
        }
    };

    if canonical_json(doc).len() > MAX_DOC_BYTES {
        d.err(
            "doc_too_large",
            "",
            None,
            format!("the blueprint exceeds {MAX_DOC_BYTES} bytes; move long prompts into fewer nodes or shorten them."),
        );
    }
    check_header(&bp, &mut d);
    let index = bp.index();
    check_nodes(&bp, doc, &index, &mut d);
    check_edges(&bp, &mut d);
    if !d.structural {
        check_cycles(&bp, &index, &mut d);
    }
    check_templates(&bp, &index, &mut d);
    check_cast(&bp, env, &mut d);

    let bounds = if d.structural {
        None
    } else {
        let worst = worst_steps(&bp, &index, None, 0);
        if worst > u64::from(bp.budget.max_steps) {
            d.warn(
                "budget_below_worst_case",
                "budget.max_steps",
                None,
                format!(
                    "the graph can take up to {worst} steps but max_steps is {}; raise max_steps or expect a budget gate.",
                    bp.budget.max_steps
                ),
            );
        }
        Some(Bounds { worst_steps: worst })
    };
    done(d, Some(bp), bounds)
}

fn check_header(bp: &Blueprint, d: &mut Diags) {
    if !is_valid_blueprint_name(&bp.name) {
        d.err(
            "invalid_name",
            "name",
            None,
            format!(
                "name {:?} must match ^[a-z0-9][a-z0-9_-]{{0,63}}$; rename it.",
                bp.name
            ),
        );
    }
    if let Some(title) = &bp.title {
        if title.len() > MAX_SHORT_BYTES {
            d.err(
                "text_too_large",
                "title",
                None,
                format!("title exceeds {MAX_SHORT_BYTES} bytes; shorten it."),
            );
        }
    }
    for name in bp.inputs.keys() {
        if !is_valid_input_name(name) {
            d.err(
                "invalid_input_name",
                format!("inputs.{name}"),
                None,
                format!("input {name:?} must match ^[a-z][a-z0-9_]{{0,31}}$; rename it."),
            );
        }
    }
    if let Some(field) = bp.budget.out_of_range() {
        d.err(
            "budget_out_of_range",
            format!("budget.{field}"),
            None,
            "budget must keep max_steps in 1..=500, max_active_secs in 60..=604800 and max_parallel in 1..=4; adjust it.".into(),
        );
    }
    if bp.workspace.mode.is_unknown() {
        unknown_value(d, "workspace.mode", None);
    }
    if bp.policy.on_error.is_unknown() {
        unknown_value(d, "policy.on_error", None);
    }
    if bp.policy.on_budget.is_unknown() {
        unknown_value(d, "policy.on_budget", None);
    }
    for (i, feature) in bp.requires.iter().enumerate() {
        if !SUPPORTED_REQUIRES.contains(&feature.as_str()) {
            d.err(
                "unsupported_feature",
                format!("requires[{i}]"),
                None,
                format!("this agentpit does not support {feature:?}; upgrade agentpit or remove the feature."),
            );
        }
    }
}

fn unknown_value(d: &mut Diags, path: &str, node: Option<&str>) {
    d.err(
        "unknown_value",
        path,
        node,
        format!("{path} has a value this agentpit does not know; use a documented value or upgrade agentpit."),
    );
}

fn check_nodes(bp: &Blueprint, doc: &Value, index: &BlueprintIndex, d: &mut Diags) {
    if bp.nodes.is_empty() {
        d.structural(
            "no_nodes",
            "nodes",
            None,
            "a blueprint needs at least one node; add an agent node.".into(),
        );
    }
    if bp.nodes.len() > MAX_NODES {
        d.structural(
            "too_many_nodes",
            "nodes",
            None,
            format!("a blueprint may have at most {MAX_NODES} nodes; split it."),
        );
    }
    let mut seen = BTreeSet::new();
    for (i, node) in bp.nodes.iter().enumerate() {
        let path = |f: &str| {
            if f.is_empty() {
                format!("nodes[{i}]")
            } else {
                format!("nodes[{i}].{f}")
            }
        };
        let id = Some(node.id.as_str());
        if !is_valid_node_id(&node.id) {
            d.structural(
                "invalid_node_id",
                path("id"),
                id,
                format!(
                    "node id {:?} must match ^[a-z][a-z0-9_-]{{0,31}}$; rename it.",
                    node.id
                ),
            );
        } else if !seen.insert(node.id.as_str()) {
            d.structural(
                "duplicate_node_id",
                path("id"),
                id,
                format!("node id {:?} is used twice; rename one of them.", node.id),
            );
        }
        if let Some(title) = &node.title {
            if title.len() > MAX_SHORT_BYTES {
                d.err(
                    "text_too_large",
                    path("title"),
                    id,
                    format!("title exceeds {MAX_SHORT_BYTES} bytes; shorten it."),
                );
            }
        }
        if let Some(parent) = &node.parent {
            match bp.node(parent) {
                None => d.structural(
                    "unknown_parent",
                    path("parent"),
                    id,
                    format!("parent {parent:?} is not a node; point it at a repeat node or remove it."),
                ),
                Some(p) if !matches!(p.spec, NodeSpec::Repeat(_)) => d.structural(
                    "parent_not_repeat",
                    path("parent"),
                    id,
                    format!("parent {parent:?} is not a repeat node; only repeat nodes contain other nodes."),
                ),
                Some(_) => {}
            }
        }
        match &node.spec {
            NodeSpec::Unsupported => {
                let raw = doc["nodes"][i]["kind"].as_str().unwrap_or("?").to_string();
                d.err(
                    "unsupported_node_kind",
                    path("kind"),
                    id,
                    format!("node kind {raw:?} cannot run in this agentpit; use agent, check, gate or repeat."),
                );
            }
            NodeSpec::Agent(a) => {
                check_text(d, &a.task, path("task"), id, "task");
                if a.role.is_some() && a.backend.is_some() {
                    d.err(
                        "cast_conflict",
                        path("role"),
                        id,
                        "an agent names either a role or a backend, not both; remove one.".into(),
                    );
                }
                if a.access.is_unknown() {
                    unknown_value(d, &path("access"), id);
                }
                check_retries(d, a.retries, path("retries"), id);
                check_timeout(d, a.timeout_secs, path("timeout_secs"), id);
            }
            NodeSpec::Check(c) => {
                if c.command.trim().is_empty() {
                    d.err(
                        "empty_text",
                        path("command"),
                        id,
                        "a check needs a command; set command (run with sh -c).".into(),
                    );
                }
                if c.command.len() > MAX_TEXT_BYTES {
                    d.err(
                        "text_too_large",
                        path("command"),
                        id,
                        format!("command exceeds {MAX_TEXT_BYTES} bytes; move it into a script."),
                    );
                }
                if let Some(cwd) = &c.cwd {
                    if !is_safe_rel_path(cwd) {
                        d.err(
                            "invalid_path",
                            path("cwd"),
                            id,
                            format!(
                                "cwd {cwd:?} must be a relative path inside the project; fix it."
                            ),
                        );
                    }
                }
                check_retries(d, c.retries, path("retries"), id);
                check_timeout(d, c.timeout_secs, path("timeout_secs"), id);
            }
            NodeSpec::Gate(g) => {
                check_text(d, &g.prompt, path("prompt"), id, "prompt");
                check_timeout(d, g.timeout_secs, path("timeout_secs"), id);
                check_gate_options(d, g, path("options"), id);
            }
            NodeSpec::Repeat(r) => {
                if !(1..=MAX_REPEAT_ITERATIONS).contains(&r.max_iterations) {
                    d.err(
                        "max_iterations_out_of_range",
                        path("max_iterations"),
                        id,
                        format!("max_iterations must be 1..={MAX_REPEAT_ITERATIONS}; set a bound."),
                    );
                }
                if r.feedback.is_unknown() {
                    unknown_value(d, &path("feedback"), id);
                }
                if index.children(&node.id).is_empty() {
                    d.structural(
                        "empty_repeat",
                        path(""),
                        id,
                        "a repeat node needs children; set their parent to this node.".into(),
                    );
                }
            }
        }
    }

    // Parent chains: cycles and depth. Checked once per node from the raw pointers.
    for (i, node) in bp.nodes.iter().enumerate() {
        let mut depth = 0;
        let mut cur = node.parent.as_deref();
        let mut visited = BTreeSet::new();
        while let Some(p) = cur {
            if !visited.insert(p) || p == node.id {
                d.structural(
                    "parent_cycle",
                    format!("nodes[{i}].parent"),
                    Some(&node.id),
                    "parent pointers form a cycle; a repeat cannot contain itself.".into(),
                );
                break;
            }
            depth += 1;
            if depth > MAX_REPEAT_NESTING {
                d.structural(
                    "nesting_too_deep",
                    format!("nodes[{i}].parent"),
                    Some(&node.id),
                    format!(
                        "repeats nest at most {MAX_REPEAT_NESTING} deep; flatten the structure."
                    ),
                );
                break;
            }
            cur = bp.node(p).and_then(|n| n.parent.as_deref());
        }
    }
}

fn check_text(d: &mut Diags, text: &str, path: String, node: Option<&str>, field: &str) {
    if text.trim().is_empty() {
        d.err(
            "empty_text",
            path,
            node,
            format!("{field} is empty; describe what this node should do."),
        );
    } else if text.len() > MAX_TEXT_BYTES {
        d.err(
            "text_too_large",
            path,
            node,
            format!("{field} exceeds {MAX_TEXT_BYTES} bytes; shorten it."),
        );
    }
}

fn check_retries(d: &mut Diags, retries: u32, path: String, node: Option<&str>) {
    if retries > MAX_RETRIES {
        d.err(
            "retries_out_of_range",
            path,
            node,
            format!("retries must be 0..={MAX_RETRIES}; lower it."),
        );
    }
}

fn check_timeout(d: &mut Diags, timeout: Option<u64>, path: String, node: Option<&str>) {
    if let Some(t) = timeout {
        if t == 0 || t > MAX_TIMEOUT_SECS {
            d.err(
                "timeout_out_of_range",
                path,
                node,
                format!("timeout_secs must be 1..={MAX_TIMEOUT_SECS}; adjust it."),
            );
        }
    }
}

fn check_gate_options(d: &mut Diags, g: &GateSpec, path: String, node: Option<&str>) {
    let options = if g.options.is_empty() {
        default_gate_options()
    } else {
        g.options.clone()
    };
    let mut problem = None;
    if options.len() > MAX_GATE_OPTIONS {
        problem = Some(format!("a gate has at most {MAX_GATE_OPTIONS} options"));
    }
    let mut ids = BTreeSet::new();
    for o in &options {
        if !is_valid_option_id(&o.id) {
            problem = Some(format!(
                "option id {:?} must match ^[a-z0-9_-]{{1,32}}$",
                o.id
            ));
        } else if !ids.insert(o.id.as_str()) {
            problem = Some(format!("option id {:?} is used twice", o.id));
        }
        if !matches!(o.outcome, Outcome::Ok | Outcome::Fail) {
            problem = Some(format!("option {:?} must map to outcome ok or fail", o.id));
        }
        if o.label.as_ref().is_some_and(|l| l.len() > 128) {
            problem = Some(format!("option {:?} has a label over 128 bytes", o.id));
        }
    }
    if let Some(t) = &g.on_timeout {
        if !ids.contains(t.as_str()) {
            problem = Some(format!("on_timeout {t:?} is not one of the options"));
        }
    }
    if let Some(p) = problem {
        d.err(
            "invalid_gate_options",
            path,
            node,
            format!("{p}; fix the gate's options."),
        );
    }
}

fn check_edges(bp: &Blueprint, d: &mut Diags) {
    let mut seen = BTreeSet::new();
    for (i, e) in bp.edges.iter().enumerate() {
        let path = |f: &str| format!("edges[{i}].{f}");
        let from = bp.node(&e.from);
        let to = bp.node(&e.to);
        if from.is_none() || to.is_none() {
            let (field, id) = if from.is_none() {
                ("from", &e.from)
            } else {
                ("to", &e.to)
            };
            d.structural(
                "unknown_edge_endpoint",
                path(field),
                None,
                format!("edge {field} {id:?} is not a node; point it at an existing node."),
            );
            continue;
        }
        let (from, to) = (from.unwrap(), to.unwrap());
        if from.parent != to.parent {
            d.structural(
                "cross_scope_edge",
                path("to"),
                Some(&e.from),
                format!(
                    "edge {} → {} crosses a repeat boundary; connect the repeat node itself instead.",
                    e.from, e.to
                ),
            );
        }
        if e.on.is_unknown() {
            unknown_value(d, &path("on"), Some(&e.from));
        } else if from.spec.kind() != NodeKind::Unknown
            && !EdgeOn::allowed_for(from.spec.kind()).contains(&e.on)
        {
            let allowed: Vec<&str> = EdgeOn::allowed_for(from.spec.kind())
                .iter()
                .map(EdgeOn::as_str)
                .collect();
            d.err(
                "edge_on_not_allowed",
                path("on"),
                Some(&e.from),
                format!(
                    "a {} node cannot route on {:?}; use one of {}.",
                    from.spec.kind(),
                    e.on.as_str(),
                    allowed.join(", ")
                ),
            );
        }
        if !seen.insert((e.from.as_str(), e.to.as_str(), e.on)) {
            d.err(
                "duplicate_edge",
                path("to"),
                Some(&e.from),
                format!(
                    "edge {} → {} on {} appears twice; remove one.",
                    e.from, e.to, e.on
                ),
            );
        }
    }
}

/// Every scope (the top level, and each repeat's children) must be a DAG: loops are
/// expressed only with `repeat`.
fn check_cycles(bp: &Blueprint, index: &BlueprintIndex, d: &mut Diags) {
    let mut scopes: Vec<Vec<String>> = vec![index.top_level()];
    for n in &bp.nodes {
        if matches!(n.spec, NodeSpec::Repeat(_)) {
            scopes.push(index.children(&n.id));
        }
    }
    for scope in scopes {
        let members: BTreeSet<&str> = scope.iter().map(String::as_str).collect();
        let mut indegree: BTreeMap<&str, usize> = members.iter().map(|m| (*m, 0)).collect();
        let mut succ: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in &bp.edges {
            if members.contains(e.from.as_str()) && members.contains(e.to.as_str()) {
                *indegree.get_mut(e.to.as_str()).unwrap() += 1;
                succ.entry(e.from.as_str()).or_default().push(e.to.as_str());
            }
        }
        let mut ready: Vec<&str> = indegree
            .iter()
            .filter(|(_, n)| **n == 0)
            .map(|(k, _)| *k)
            .collect();
        let mut removed = 0;
        while let Some(n) = ready.pop() {
            removed += 1;
            for s in succ.get(n).cloned().unwrap_or_default() {
                let deg = indegree.get_mut(s).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    ready.push(s);
                }
            }
        }
        if removed < members.len() {
            let stuck: Vec<&str> = indegree
                .iter()
                .filter(|(_, n)| **n > 0)
                .map(|(k, _)| *k)
                .collect();
            d.structural(
                "cycle",
                "edges",
                stuck.first().copied(),
                format!(
                    "edges form a cycle through {}; wrap the cycle in a repeat node.",
                    stuck.join(", ")
                ),
            );
        }
    }
}

fn check_templates(bp: &Blueprint, index: &BlueprintIndex, d: &mut Diags) {
    for (i, node) in bp.nodes.iter().enumerate() {
        let (field, template, is_agent) = match &node.spec {
            NodeSpec::Agent(a) => ("task", a.task.as_str(), true),
            NodeSpec::Gate(g) => ("prompt", g.prompt.as_str(), false),
            _ => continue,
        };
        let path = format!("nodes[{i}].{field}");
        let id = Some(node.id.as_str());
        let tokens = match placeholders(template) {
            Ok(tokens) => tokens,
            Err(at) => {
                d.err(
                    "invalid_placeholder",
                    path,
                    id,
                    format!("{field} has an unterminated {{{{ at byte {at}; close it with }}}}."),
                );
                continue;
            }
        };
        let in_repeat = !index.repeat_chain(&node.id).is_empty();
        for token in tokens {
            let parts: Vec<&str> = token.split('.').collect();
            match parts.as_slice() {
                ["goal"] => {
                    if !bp.inputs.contains_key("goal") {
                        d.err(
                            "unknown_input",
                            path.clone(),
                            id,
                            "{{goal}} needs an input named goal; declare it under inputs.".into(),
                        );
                    }
                }
                ["inputs", name] => {
                    if !bp.inputs.contains_key(*name) {
                        d.err(
                            "unknown_input",
                            path.clone(),
                            id,
                            format!("{{{{inputs.{name}}}}} names no declared input; declare it under inputs."),
                        );
                    }
                }
                ["iteration"] | ["feedback"] if !in_repeat => d.err(
                    "invalid_placeholder",
                    path.clone(),
                    id,
                    format!("{{{{{token}}}}} only exists inside a repeat; move this node into one or drop it."),
                ),
                ["iteration"] | ["feedback"] => {}
                ["instructions"] if !is_agent => d.err(
                    "invalid_placeholder",
                    path.clone(),
                    id,
                    "{{instructions}} is only available in agent tasks; remove it from the gate prompt.".into(),
                ),
                ["instructions"] => {}
                ["nodes", target, "output" | "outcome"] => {
                    if *target == node.id {
                        d.err(
                            "invalid_placeholder",
                            path.clone(),
                            id,
                            "a node cannot read its own output; reference an upstream node.".into(),
                        );
                    } else if !index.contains(target) {
                        d.err(
                            "unknown_template_node",
                            path.clone(),
                            id,
                            format!("{{{{{token}}}}} names no node; fix the node id."),
                        );
                    }
                }
                _ => d.err(
                    "invalid_placeholder",
                    path.clone(),
                    id,
                    format!(
                        "{{{{{token}}}}} is not a placeholder; use goal, inputs.<name>, iteration, feedback, instructions or nodes.<id>.output|outcome."
                    ),
                ),
            }
        }
    }
}

fn check_cast(bp: &Blueprint, env: &ValidateEnv, d: &mut Diags) {
    for (i, node) in bp.nodes.iter().enumerate() {
        let NodeSpec::Agent(a) = &node.spec else {
            continue;
        };
        if let (Some(role), Some(roles)) = (&a.role, &env.roles) {
            if !roles.contains(role) {
                d.warn(
                    "unknown_role",
                    format!("nodes[{i}].role"),
                    Some(&node.id),
                    format!(
                        "role {role:?} is not in [workflow.roles]; add it or pick another role."
                    ),
                );
            }
        }
        if let (Some(backend), Some(backends)) = (&a.backend, &env.backends) {
            if !backends.contains(backend) {
                d.warn(
                    "unknown_backend",
                    format!("nodes[{i}].backend"),
                    Some(&node.id),
                    format!("backend {backend:?} is not known; pick an installed backend."),
                );
            }
        }
    }
}

/// `worst(scope) = Σ w(n)`; saturating so an absurd document cannot overflow.
fn worst_steps(bp: &Blueprint, index: &BlueprintIndex, scope: Option<&str>, depth: usize) -> u64 {
    if depth > MAX_REPEAT_NESTING {
        return 0;
    }
    let members = match scope {
        None => index.top_level(),
        Some(r) => index.children(r),
    };
    members
        .iter()
        .filter_map(|id| bp.node(id))
        .fold(0u64, |acc, n| {
            let w = match &n.spec {
                NodeSpec::Agent(a) => 1 + u64::from(a.retries),
                NodeSpec::Check(c) => 1 + u64::from(c.retries),
                NodeSpec::Gate(_) | NodeSpec::Unsupported => 0,
                NodeSpec::Repeat(r) => u64::from(r.max_iterations).saturating_mul(worst_steps(
                    bp,
                    index,
                    Some(&n.id),
                    depth + 1,
                )),
            };
            acc.saturating_add(w)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/loops/blueprint_fix_until_green.json"
        ))
        .unwrap()
    }

    fn minimal() -> Value {
        json!({
            "schema": "agentpit.blueprint/1",
            "name": "t",
            "inputs": {"goal": {"required": true}},
            "nodes": [{"id": "a", "kind": "agent", "task": "do {{goal}}"}]
        })
    }

    fn codes_of(doc: &Value) -> Vec<String> {
        validate(doc, &ValidateEnv::default())
            .diagnostics
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn fixture_blueprint_validates_clean_with_bounds_9() {
        let v = validate(&fixture(), &ValidateEnv::default());
        assert!(v.diagnostics.is_empty(), "{:?}", v.diagnostics);
        assert!(v.is_runnable());
        // plan (1) + fix: 4 iterations × (implement 1 + test 1)
        assert_eq!(v.bounds, Some(Bounds { worst_steps: 9 }));
        let bp = v.blueprint.unwrap();
        assert!(bp.is_understood());
        assert_eq!(
            bp.required_features().into_iter().collect::<Vec<_>>(),
            vec![
                "bp.node.agent",
                "bp.node.check",
                "bp.node.gate",
                "bp.node.repeat",
                "bp.workspace.in_place"
            ]
        );
    }

    #[test]
    fn blueprint_rev_is_pinned_and_ignores_layout_and_key_order() {
        let doc = fixture();
        let rev = blueprint_rev(&doc);
        assert!(rev.starts_with("b1-") && rev.len() == 19, "{rev}");

        let mut moved = doc.clone();
        moved["layout"] = json!({"plan": {"x": 999, "y": 1}});
        assert_eq!(blueprint_rev(&moved), rev, "dragging a node keeps the rev");

        let mut edited = doc;
        edited["budget"]["max_steps"] = json!(13);
        assert_ne!(
            blueprint_rev(&edited),
            rev,
            "a semantic edit changes the rev"
        );

        assert_eq!(
            blueprint_rev(&json!({"b": 1, "a": [2, {"d": 3, "c": 4}]})),
            blueprint_rev(&json!({"a": [2, {"c": 4, "d": 3}], "b": 1})),
        );
    }

    #[test]
    fn canonical_json_sorts_nested_keys() {
        assert_eq!(
            canonical_json(&json!({"b": {"y": 1, "x": [true, null]}, "a": "é"})),
            r#"{"a":"é","b":{"x":[true,null],"y":1}}"#
        );
    }

    #[test]
    fn each_error_code_is_reachable() {
        let base = minimal();
        let with = |f: &dyn Fn(&mut Value)| {
            let mut d = base.clone();
            f(&mut d);
            d
        };
        let cases: Vec<(&str, Value)> = vec![
            ("invalid_shape", json!([1])),
            (
                "schema_unsupported",
                with(&|d| d["schema"] = json!("agentpit.blueprint/2")),
            ),
            ("invalid_shape", with(&|d| d["nodes"] = json!("x"))),
            (
                "doc_too_large",
                with(&|d| d["description"] = json!("x".repeat(MAX_DOC_BYTES))),
            ),
            ("invalid_name", with(&|d| d["name"] = json!("Bad Name"))),
            (
                "invalid_input_name",
                with(&|d| d["inputs"]["Goal"] = json!({})),
            ),
            (
                "budget_out_of_range",
                with(&|d| d["budget"] = json!({"max_parallel": 9})),
            ),
            (
                "unknown_value",
                with(&|d| d["workspace"] = json!({"mode": "vm"})),
            ),
            (
                "unsupported_feature",
                with(&|d| d["requires"] = json!(["bp.expr"])),
            ),
            ("no_nodes", with(&|d| d["nodes"] = json!([]))),
            (
                "too_many_nodes",
                with(&|d| {
                    d["nodes"] = json!((0..65)
                        .map(|i| json!({"id": format!("n{i}"), "kind": "check", "command": "true"}))
                        .collect::<Vec<_>>())
                }),
            ),
            (
                "invalid_node_id",
                with(&|d| d["nodes"][0]["id"] = json!("A.b")),
            ),
            (
                "duplicate_node_id",
                with(&|d| {
                    d["nodes"] = json!([
                        {"id": "a", "kind": "check", "command": "true"},
                        {"id": "a", "kind": "check", "command": "true"}
                    ])
                }),
            ),
            (
                "unsupported_node_kind",
                with(&|d| d["nodes"][0]["kind"] = json!("manager")),
            ),
            (
                "unknown_parent",
                with(&|d| d["nodes"][0]["parent"] = json!("nope")),
            ),
            (
                "parent_not_repeat",
                with(&|d| {
                    d["nodes"] = json!([
                        {"id": "a", "kind": "check", "command": "true"},
                        {"id": "b", "kind": "check", "command": "true", "parent": "a"}
                    ])
                }),
            ),
            (
                "parent_cycle",
                with(&|d| {
                    d["nodes"] = json!([
                        {"id": "r", "kind": "repeat", "max_iterations": 2, "parent": "s"},
                        {"id": "s", "kind": "repeat", "max_iterations": 2, "parent": "r"}
                    ])
                }),
            ),
            (
                "nesting_too_deep",
                with(&|d| {
                    d["nodes"] = json!([
                        {"id": "r1", "kind": "repeat", "max_iterations": 2},
                        {"id": "r2", "kind": "repeat", "max_iterations": 2, "parent": "r1"},
                        {"id": "r3", "kind": "repeat", "max_iterations": 2, "parent": "r2"},
                        {"id": "r4", "kind": "repeat", "max_iterations": 2, "parent": "r3"},
                        {"id": "x", "kind": "check", "command": "true", "parent": "r4"}
                    ])
                }),
            ),
            (
                "empty_repeat",
                with(&|d| d["nodes"] = json!([{"id": "r", "kind": "repeat", "max_iterations": 2}])),
            ),
            (
                "max_iterations_out_of_range",
                with(&|d| {
                    d["nodes"] = json!([
                        {"id": "r", "kind": "repeat", "max_iterations": 21},
                        {"id": "x", "kind": "check", "command": "true", "parent": "r"}
                    ])
                }),
            ),
            ("empty_text", with(&|d| d["nodes"][0]["task"] = json!("  "))),
            (
                "text_too_large",
                with(&|d| d["nodes"][0]["task"] = json!("x".repeat(MAX_TEXT_BYTES + 1))),
            ),
            (
                "cast_conflict",
                with(&|d| {
                    d["nodes"][0]["role"] = json!("coder");
                    d["nodes"][0]["backend"] = json!("claude");
                }),
            ),
            (
                "retries_out_of_range",
                with(&|d| d["nodes"][0]["retries"] = json!(4)),
            ),
            (
                "timeout_out_of_range",
                with(&|d| d["nodes"][0]["timeout_secs"] = json!(0)),
            ),
            (
                "invalid_path",
                with(&|d| {
                    d["nodes"] =
                        json!([{"id": "c", "kind": "check", "command": "true", "cwd": "../x"}])
                }),
            ),
            (
                "invalid_gate_options",
                with(&|d| {
                    d["nodes"] = json!([{"id": "g", "kind": "gate", "prompt": "ok?",
                        "options": [{"id": "a", "outcome": "ok"}], "on_timeout": "b"}])
                }),
            ),
            (
                "unknown_edge_endpoint",
                with(&|d| d["edges"] = json!([{"from": "a", "to": "zz"}])),
            ),
            (
                "cross_scope_edge",
                with(&|d| {
                    d["nodes"] = json!([
                        {"id": "a", "kind": "check", "command": "true"},
                        {"id": "r", "kind": "repeat", "max_iterations": 2},
                        {"id": "b", "kind": "check", "command": "true", "parent": "r"}
                    ]);
                    d["edges"] = json!([{"from": "a", "to": "b"}]);
                }),
            ),
            (
                "edge_on_not_allowed",
                with(&|d| {
                    d["nodes"] = json!([
                        {"id": "g", "kind": "gate", "prompt": "ok?"},
                        {"id": "b", "kind": "check", "command": "true"}
                    ]);
                    d["edges"] = json!([{"from": "g", "to": "b", "on": "error"}]);
                }),
            ),
            (
                "duplicate_edge",
                with(&|d| {
                    d["nodes"] = json!([
                        {"id": "a", "kind": "check", "command": "true"},
                        {"id": "b", "kind": "check", "command": "true"}
                    ]);
                    d["edges"] =
                        json!([{"from": "a", "to": "b"}, {"from": "a", "to": "b", "on": "ok"}]);
                }),
            ),
            (
                "cycle",
                with(&|d| {
                    d["nodes"] = json!([
                        {"id": "a", "kind": "check", "command": "true"},
                        {"id": "b", "kind": "check", "command": "true"}
                    ]);
                    d["edges"] =
                        json!([{"from": "a", "to": "b"}, {"from": "b", "to": "a", "on": "fail"}]);
                }),
            ),
            (
                "invalid_placeholder",
                with(&|d| d["nodes"][0]["task"] = json!("{{iteration}}")),
            ),
            (
                "unknown_input",
                with(&|d| d["nodes"][0]["task"] = json!("{{inputs.path}}")),
            ),
            (
                "unknown_template_node",
                with(&|d| d["nodes"][0]["task"] = json!("{{nodes.zz.output}}")),
            ),
        ];
        for (code, doc) in cases {
            let codes = codes_of(&doc);
            assert!(codes.iter().any(|c| c == code), "{code}: got {codes:?}");
        }
    }

    #[test]
    fn unsupported_kind_parses_keeps_unknown_keys_and_blocks_run() {
        let mut doc = minimal();
        doc["nodes"][0] = json!({"id": "m", "kind": "manager", "goal": "x", "future_key": 1});
        doc["future_top"] = json!({"a": 1});
        let v = validate(&doc, &ValidateEnv::default());
        let bp = v
            .blueprint
            .as_ref()
            .expect("typed view survives an unknown kind");
        assert_eq!(bp.nodes[0].spec, NodeSpec::Unsupported);
        assert!(!bp.is_understood());
        assert!(!v.is_runnable());
        assert_eq!(v.codes(), vec!["unsupported_node_kind"]);
    }

    #[test]
    fn placeholders_parse_and_report_unterminated_braces() {
        assert_eq!(
            placeholders("a {{ goal }} b {{nodes.plan.output}}").unwrap(),
            vec!["goal", "nodes.plan.output"]
        );
        assert_eq!(placeholders("x {{goal"), Err(2));
        assert!(placeholders("no tokens").unwrap().is_empty());
    }

    #[test]
    fn instructions_are_agent_only_and_self_reference_is_refused() {
        let mut doc = minimal();
        doc["nodes"] = json!([
            {"id": "a", "kind": "agent", "task": "{{nodes.a.output}}"},
            {"id": "g", "kind": "gate", "prompt": "{{instructions}}"}
        ]);
        let codes = codes_of(&doc);
        assert_eq!(codes, vec!["invalid_placeholder", "invalid_placeholder"]);
    }

    #[test]
    fn nested_repeat_bounds_multiply_and_low_budget_warns() {
        let doc = json!({
            "schema": "agentpit.blueprint/1",
            "name": "nested",
            "budget": {"max_steps": 10},
            "nodes": [
                {"id": "outer", "kind": "repeat", "max_iterations": 3},
                {"id": "inner", "kind": "repeat", "max_iterations": 2, "parent": "outer"},
                {"id": "work", "kind": "agent", "task": "x", "retries": 1, "parent": "inner"},
                {"id": "ok", "kind": "gate", "prompt": "fine?", "parent": "outer"}
            ],
            "edges": [{"from": "inner", "to": "ok"}]
        });
        let v = validate(&doc, &ValidateEnv::default());
        assert_eq!(v.bounds, Some(Bounds { worst_steps: 12 }));
        assert_eq!(v.codes(), vec!["budget_below_worst_case"]);
        assert!(v.is_runnable(), "warnings do not block a run");
    }

    #[test]
    fn unknown_roles_and_backends_only_warn() {
        let mut doc = minimal();
        doc["nodes"][0]["role"] = json!("ghost");
        let env = ValidateEnv {
            roles: Some(["coder".to_string()].into()),
            backends: None,
        };
        let v = validate(&doc, &env);
        assert_eq!(v.codes(), vec!["unknown_role"]);
        assert!(v.is_runnable());
    }

    #[test]
    fn repeat_chain_and_children_follow_parent_pointers() {
        let bp: Blueprint = serde_json::from_value(fixture()).unwrap();
        let ix = bp.index();
        assert_eq!(ix.repeat_chain("test"), vec!["fix"]);
        assert!(ix.repeat_chain("plan").is_empty());
        assert_eq!(ix.children("fix"), vec!["implement", "test"]);
        assert_eq!(ix.top_level(), vec!["plan", "fix", "signoff"]);
        assert!(ix.is_descendant("implement", "fix"));
    }

    #[test]
    fn index_walks_terminate_on_parent_cycles() {
        let bp: Blueprint = serde_json::from_value(json!({
            "schema": "agentpit.blueprint/1", "name": "c",
            "nodes": [
                {"id": "r", "kind": "repeat", "max_iterations": 2, "parent": "s"},
                {"id": "s", "kind": "repeat", "max_iterations": 2, "parent": "r"}
            ]
        }))
        .unwrap();
        let chain = bp.index().repeat_chain("r");
        assert!(chain.len() <= 2, "{chain:?}");
    }

    #[test]
    fn edge_on_matching_never_routes_cancelled() {
        assert!(EdgeOn::NotOk.matches(Outcome::Exhausted));
        assert!(!EdgeOn::NotOk.matches(Outcome::Ok));
        assert!(EdgeOn::Always.matches(Outcome::Fail));
        assert!(!EdgeOn::Always.matches(Outcome::Cancelled));
        assert!(!EdgeOn::Unknown.matches(Outcome::Ok));
        let e: Edge = serde_json::from_value(json!({"from": "a", "to": "b"})).unwrap();
        assert_eq!(e.on, EdgeOn::Ok, "an edge without on fires on ok");
    }
}
