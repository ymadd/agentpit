import type { HubConfig, RouteKey } from "./config.js";
import type { BackendId } from "./types.js";

export interface RouteRequest {
  readonly tool: RouteKey;
  readonly explicitBackend?: BackendId;
  readonly task?: string;
}

export interface RouteDecision {
  readonly backend: BackendId;
  readonly reason:
    | "explicit"
    | "route_table"
    | "auto_long_context"
    | "auto_keyword"
    | "default";
}

const estimateTokens = (text: string): number => Math.ceil(text.length / 4);

const containsAnyKeyword = (text: string, keywords: ReadonlyArray<string>): boolean => {
  const lower = text.toLowerCase();
  return keywords.some((keyword) => lower.includes(keyword.toLowerCase()));
};

export class Router {
  constructor(
    private readonly config: HubConfig,
    private readonly available: ReadonlySet<BackendId>,
  ) {}

  resolve(request: RouteRequest): RouteDecision {
    if (request.explicitBackend && this.available.has(request.explicitBackend)) {
      return { backend: request.explicitBackend, reason: "explicit" };
    }

    const routed = this.config.routes[request.tool];
    if (routed && this.available.has(routed)) {
      return { backend: routed, reason: "route_table" };
    }

    if (this.config.default.auto_route && request.task) {
      const auto = this.config.auto_route;
      if (
        this.available.has(auto.long_context_backend) &&
        estimateTokens(request.task) > auto.long_context_threshold
      ) {
        return {
          backend: auto.long_context_backend,
          reason: "auto_long_context",
        };
      }
      if (
        this.available.has(auto.review_backend) &&
        containsAnyKeyword(request.task, auto.review_keywords)
      ) {
        return { backend: auto.review_backend, reason: "auto_keyword" };
      }
    }

    const fallback = this.config.default.backend;
    const finalBackend = this.available.has(fallback)
      ? fallback
      : (this.available.values().next().value as BackendId);
    return { backend: finalBackend, reason: "default" };
  }
}
