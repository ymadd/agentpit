import { test } from "node:test";
import assert from "node:assert/strict";
import { draftFromSettings, validate, roleNameError, typeNameError, buildPayload, newRole, newType, DEFAULT_MAX_DEPTH } from "./settings.js";

const raw = {
  known_backends: ["claude", "codex", "antigravity"],
  backend_models: { claude: "claude-fable-5", codex: null, antigravity: "gemini-3-pro" },
  workflow: { manager_backend: "claude", max_depth: 4, max_calls_per_manager: 10, use_mcp: true, enable_ask_human: false },
  roles: [{ name: "coder", backends: ["codex"], prompt: "be precise", model: "gpt-5-codex" }],
  types: [{ name: "review", title: "Review", roles: ["coder"], max_depth: 2 }],
};

test("draftFromSettings makes an editable draft with defaults", () => {
  const d = draftFromSettings(raw);
  assert.equal(d.workflow.manager_backend, "claude");
  assert.equal(d.workflow.max_depth, 4);
  assert.equal(d.roles.length, 1);
  assert.ok(typeof d.roles[0]._key === "number");
  assert.equal(d.roles[0].model, "gpt-5-codex");
  assert.equal(d.backend_models.claude, "claude-fable-5");
  assert.equal(d.backend_models.codex, "");
  // an empty config still gets sane knob defaults
  assert.equal(draftFromSettings({}).workflow.max_depth, DEFAULT_MAX_DEPTH);
});

test("roleNameError enforces the backend name rules", () => {
  const roles = [{ _key: 1, name: "coder" }, { _key: 2, name: "reviewer" }];
  assert.equal(roleNameError("coder", roles, 1), null); // itself, ok
  assert.equal(roleNameError("", roles, 3), "Enter a name");
  assert.ok(roleNameError("Bad Name", roles, 3)); // spaces/uppercase
  assert.ok(roleNameError("coder", roles, 3)); // duplicate
});

test("validate flags a duplicate role name", () => {
  const draft = { roles: [newRole(), newRole()] };
  draft.roles[0].name = "dup";
  draft.roles[1].name = "dup";
  assert.equal(validate(draft).ok, false);
  draft.roles[1].name = "other";
  assert.equal(validate(draft).ok, true);
});

test("typeNameError enforces reserved names, regex, and uniqueness", () => {
  const types = [{ _key: 1, name: "review" }, { _key: 2, name: "harden" }];
  assert.equal(typeNameError("review", types, 1), null); // itself, ok
  assert.equal(typeNameError("", types, 3), "Enter a name");
  assert.ok(typeNameError("new", types, 3)); // reserved
  assert.ok(typeNameError("list", types, 3)); // reserved
  assert.ok(typeNameError("Bad Name", types, 3)); // regex
  assert.ok(typeNameError("review", types, 3)); // duplicate
});

test("validate flags an invalid workflow type (unnamed / reserved)", () => {
  const draft = { roles: [], types: [newType()] };
  assert.equal(validate(draft).ok, false); // unnamed new type
  draft.types[0].name = "describe";
  assert.equal(validate(draft).ok, false); // reserved
  draft.types[0].name = "code-review";
  assert.equal(validate(draft).ok, true);
});

test("buildPayload matches the settings_save contract (nulls, round-tripped types)", () => {
  const p = buildPayload(draftFromSettings(raw));
  assert.equal(p.workflow.manager_backend, "claude");
  assert.equal(p.workflow.max_depth, 4);
  // Backend defaults are owned by the top-level Settings → Backends save path.
  // A stale mounted Studio must never overwrite them.
  assert.equal(p.backend_models, undefined);
  assert.deepEqual(p.roles[0], { name: "coder", backends: ["codex"], prompt: "be precise", model: "gpt-5-codex" });
  // empty model → null
  const d2 = draftFromSettings({ roles: [{ name: "r", backends: [], prompt: "" }] });
  assert.equal(buildPayload(d2).roles[0].model, null);
  assert.equal(buildPayload(d2).backend_models, undefined);
  // types survive Save unchanged
  assert.equal(p.types[0].name, "review");
  assert.equal(p.types[0].max_depth, 2);
  assert.equal(p.types[0].title, "Review");
});

// The BASE canvas's sketch reaches config through `[workflow].flow`. Blank must send null
// so the Rust side removes the key ("unset = no hint") instead of writing an empty string.
test("buildPayload carries the base workflow flow hint, blank as null", () => {
  const d = draftFromSettings({ ...raw, workflow: { ...raw.workflow, flow: "Diagnose → Plan" } });
  assert.equal(d.workflow.flow, "Diagnose → Plan");
  assert.equal(buildPayload(d).workflow.flow, "Diagnose → Plan");

  d.workflow.flow = "   ";
  assert.equal(buildPayload(d).workflow.flow, null);
  assert.equal(buildPayload(draftFromSettings(raw)).workflow.flow, null);
});

test("buildPayload carries the plan steps for the base workflow and each type", () => {
  const step = { name: "Review", persona: null, behavior: null, manager_backend: "claude", roles: ["reviewer"], backends: [], fanout: 2, dynamic: true, ask: false };
  const d = draftFromSettings({
    ...raw,
    workflow: { ...raw.workflow, steps: [step] },
    types: [{ name: "review", steps: [{ ...step, name: "Audit" }] }],
  });
  assert.deepEqual(d.workflow.steps, [step]);
  assert.equal(d.types[0].steps[0].name, "Audit");

  const payload = buildPayload(d);
  assert.deepEqual(payload.workflow.steps, [step]);
  assert.equal(payload.types[0].steps[0].name, "Audit");

  // no plan sketched → an empty array, which the Rust side turns into "remove the key"
  assert.deepEqual(buildPayload(draftFromSettings(raw)).workflow.steps, []);
  assert.deepEqual(buildPayload(draftFromSettings(raw)).types[0].steps, []);
});
