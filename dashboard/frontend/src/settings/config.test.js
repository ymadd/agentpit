import test from "node:test";
import assert from "node:assert/strict";

import { buildConfigPayload, draftFromConfig, validateConfigDraft } from "./config.js";

test("full config draft retains every CLI settings section", () => {
  const draft = draftFromConfig({
    known_backends: ["claude", "codex"],
    defaults: { backend: "codex", auto_route: false },
    routes: { rescue: "claude", review: "codex", explain: "claude", refactor: "codex" },
    auto_route: {
      long_context_threshold: 250000,
      long_context_backend: "claude",
      review_keywords: ["review", "security"],
      review_backend: "codex",
    },
    ensemble: {
      default: { members: ["claude", "codex"], aggregator: "claude" },
    },
    backends: [{ id: "codex", transport: "exec", model: "gpt-test" }],
  });

  assert.equal(draft.defaults.backend, "codex");
  assert.equal(draft.routes.review, "codex");
  assert.equal(draft.auto_route.review_keywords_text, "review, security");
  assert.deepEqual(draft.ensemble.default.members, ["claude", "codex"]);
  assert.equal(draft.backends[1].model, "gpt-test");
});

test("save payload trims models and normalizes keywords", () => {
  const draft = draftFromConfig({ known_backends: ["claude", "codex"] });
  draft.auto_route.review_keywords_text = " review, security, review,  ";
  draft.backends[0].transport = "acp";
  draft.backends[0].model = "  opus  ";
  draft.backends[1].model = "   ";

  const payload = buildConfigPayload(draft);
  assert.deepEqual(payload.auto_route.review_keywords, ["review", "security"]);
  assert.deepEqual(payload.backends[0], { id: "claude", transport: "acp", model: "opus" });
  assert.equal(payload.backends[1].model, null);
});

test("absent routes stay unpinned and survive the save payload", () => {
  const draft = draftFromConfig({});
  assert.deepEqual(draft.routes, { rescue: "", review: "", explain: "", refactor: "" });
  assert.equal(draft.defaults.backend, "claude");
  assert.equal(draft.auto_route.long_context_backend, "claude");
  assert.equal(validateConfigDraft(draft), null);

  draft.routes.review = "codex";
  const payload = buildConfigPayload(draft);
  assert.deepEqual(payload.routes, { rescue: "", review: "codex", explain: "", refactor: "" });
});
