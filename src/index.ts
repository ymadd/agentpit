import { startMcpServer } from "./mcp-server.js";

const main = async (): Promise<void> => {
  const mode = process.argv[2] ?? "mcp";
  switch (mode) {
    case "mcp":
      await startMcpServer();
      break;
    case "acp":
      console.error(
        "ACP server entry is not implemented in v0.1. Use 'mcp' mode.",
      );
      process.exit(2);
      break;
    default:
      console.error(`Unknown mode: ${mode}. Usage: agentpit {mcp|acp}`);
      process.exit(2);
  }
};

main().catch((error: unknown) => {
  console.error("[agentpit] fatal:", error);
  process.exit(1);
});
