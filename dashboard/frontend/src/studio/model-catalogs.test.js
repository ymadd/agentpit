import test from "node:test";
import assert from "node:assert/strict";

import { indexModelCatalogs, primaryRoleCatalog } from "./model-catalogs.js";

test("indexModelCatalogs ignores malformed rows and deduplicates model values", () => {
  const indexed = indexModelCatalogs([
    {
      backend: "codex",
      kind: "cli",
      source: "codex debug models",
      models: [
        { value: " gpt-a ", label: "GPT A" },
        { value: "gpt-a", label: "duplicate" },
        { value: "", label: "empty" },
      ],
    },
    null,
  ]);
  assert.deepEqual(indexed.codex.models, [{ value: "gpt-a", label: "GPT A" }]);
});

test("primaryRoleCatalog follows backend preference order", () => {
  const indexed = indexModelCatalogs([
    { backend: "claude", models: [{ value: "sonnet", label: "Sonnet" }] },
    { backend: "codex", models: [{ value: "gpt-a", label: "GPT A" }] },
  ]);
  assert.equal(primaryRoleCatalog(indexed, ["codex", "claude"]), indexed.codex);
  assert.equal(primaryRoleCatalog(indexed, []), null);
});
