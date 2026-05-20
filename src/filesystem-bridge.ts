import { readFile, writeFile, mkdir } from "node:fs/promises";
import { dirname, isAbsolute } from "node:path";
import type {
  ReadTextFileRequest,
  ReadTextFileResponse,
  WriteTextFileRequest,
  WriteTextFileResponse,
} from "@agentclientprotocol/sdk";

const enforceAbsolute = (path: string): string => {
  if (!isAbsolute(path)) {
    throw new Error(`Path must be absolute (received: ${path})`);
  }
  return path;
};

export const readTextFile = async (
  params: ReadTextFileRequest,
): Promise<ReadTextFileResponse> => {
  try {
    const path = enforceAbsolute(params.path);
    const content = await readFile(path, "utf8");
    if (params.line != null || params.limit != null) {
      const lines = content.split("\n");
      const start = params.line != null ? Math.max(0, params.line - 1) : 0;
      const end = params.limit != null ? start + params.limit : lines.length;
      return { content: lines.slice(start, end).join("\n") };
    }
    return { content };
  } catch (error) {
    const message =
      error instanceof Error ? error.message : "Unknown read error";
    throw new Error(`fs/read_text_file failed for ${params.path}: ${message}`);
  }
};

export const writeTextFile = async (
  params: WriteTextFileRequest,
): Promise<WriteTextFileResponse> => {
  try {
    const path = enforceAbsolute(params.path);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, params.content, "utf8");
    return {};
  } catch (error) {
    const message =
      error instanceof Error ? error.message : "Unknown write error";
    throw new Error(`fs/write_text_file failed for ${params.path}: ${message}`);
  }
};
