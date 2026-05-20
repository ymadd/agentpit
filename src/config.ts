import { readFile, writeFile, mkdir } from "node:fs/promises";
import { homedir } from "node:os";
import { join, dirname } from "node:path";
import { parse as parseToml } from "smol-toml";
import { z } from "zod";

import { BackendIdSchema } from "./types.js";

const RouteKeySchema = z.enum(["rescue", "review", "explain", "refactor"]);

export type RouteKey = z.infer<typeof RouteKeySchema>;

const ConfigSchema = z.object({
  default: z
    .object({
      backend: BackendIdSchema.default("gemini"),
      auto_route: z.boolean().default(true),
    })
    .default({}),
  routes: z
    .record(RouteKeySchema, BackendIdSchema)
    .default({
      rescue: "gemini",
      review: "claude",
      explain: "gemini",
      refactor: "claude",
    }),
  auto_route: z
    .object({
      long_context_threshold: z.number().int().positive().default(100_000),
      long_context_backend: BackendIdSchema.default("gemini"),
      review_keywords: z
        .array(z.string())
        .default(["review", "audit", "critique", "security"]),
      review_backend: BackendIdSchema.default("claude"),
    })
    .default({}),
  ensemble: z
    .object({
      default_members: z
        .array(BackendIdSchema)
        .default(["gemini", "claude", "opencode"]),
      aggregator: BackendIdSchema.optional(),
      review_members: z
        .array(BackendIdSchema)
        .default(["gemini", "opencode"]),
      review_aggregator: BackendIdSchema.optional(),
    })
    .default({}),
  backends: z
    .record(
      BackendIdSchema,
      z.object({ transport: z.enum(["exec", "acp"]).optional() }),
    )
    .default({}),
});

export type HubConfig = z.infer<typeof ConfigSchema>;

const DEFAULT_CONFIG: HubConfig = ConfigSchema.parse({
  default: { backend: "gemini", auto_route: true },
});

const DEFAULT_CONFIG_TOML = `# agentpit config
# Backends currently available: gemini, claude (codex requires a paid plan)

[default]
backend = "gemini"
auto_route = true

[routes]
rescue   = "gemini"
review   = "claude"
explain  = "gemini"
refactor = "claude"

[auto_route]
long_context_threshold = 100000
long_context_backend   = "gemini"
review_keywords        = ["review", "audit", "critique", "security"]
review_backend         = "claude"

[ensemble]
# Generic ensemble members + optional aggregator (leave commented to skip aggregation)
default_members = ["gemini", "claude", "opencode"]
# aggregator = "claude"

# Per-tool overrides
review_members = ["gemini", "opencode"]
# review_aggregator = "claude"

# Per-backend transport. Default behavior:
#   gemini    = exec  (CLI direct, uses Gemini OAuth)
#   claude    = exec  (CLI direct, uses subscription OAuth — switching to acp REQUIRES ANTHROPIC_API_KEY)
#   opencode  = acp   (no exec path)
# Uncomment to override:
# [backends.gemini]
# transport = "acp"
# [backends.claude]
# transport = "acp"
`;

const expandEnv = (input: unknown): unknown => {
  if (typeof input === "string") {
    return input.replace(/\$\{([A-Z0-9_]+)\}/gi, (_, name: string) => {
      return process.env[name] ?? "";
    });
  }
  if (Array.isArray(input)) {
    return input.map(expandEnv);
  }
  if (input && typeof input === "object") {
    return Object.fromEntries(
      Object.entries(input as Record<string, unknown>).map(([k, v]) => [
        k,
        expandEnv(v),
      ]),
    );
  }
  return input;
};

const xdgConfigHome = (): string =>
  process.env.XDG_CONFIG_HOME ?? join(homedir(), ".config");

export const defaultConfigPath = (): string =>
  join(xdgConfigHome(), "agentpit", "config.toml");

const readIfExists = async (path: string): Promise<string | undefined> => {
  try {
    return await readFile(path, "utf8");
  } catch (error) {
    if (
      error instanceof Error &&
      "code" in error &&
      (error as NodeJS.ErrnoException).code === "ENOENT"
    ) {
      return undefined;
    }
    throw error;
  }
};

export const writeDefaultConfig = async (path: string): Promise<void> => {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, DEFAULT_CONFIG_TOML, "utf8");
};

export interface LoadConfigResult {
  readonly config: HubConfig;
  readonly source: "file" | "defaults";
  readonly path: string;
}

export const loadConfig = async (
  pathOverride?: string,
): Promise<LoadConfigResult> => {
  const path = pathOverride ?? defaultConfigPath();
  const raw = await readIfExists(path);
  if (raw === undefined) {
    return { config: DEFAULT_CONFIG, source: "defaults", path };
  }
  try {
    const parsed = parseToml(raw);
    const expanded = expandEnv(parsed);
    const config = ConfigSchema.parse(expanded);
    return { config, source: "file", path };
  } catch (error) {
    const message =
      error instanceof Error ? error.message : "Unknown config error";
    throw new Error(`Failed to load ${path}: ${message}`);
  }
};

export { DEFAULT_CONFIG };
