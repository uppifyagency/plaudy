#!/usr/bin/env bun
// Plaude Local — local MCP server.
//
// A dependency-free Model Context Protocol server (newline-delimited JSON-RPC 2.0 over stdio)
// that lets Claude connect to your private recordings and summarize sessions or find what was
// said — without anything leaving the Mac. Read-only over the app's history.db.
//
// Register it in .mcp.json (already done for this repo) or any MCP client:
//   { "command": "bun", "args": ["run", "handy/mcp/server.ts"] }
//
// ponytail: the MCP stdio handshake is small and stable, so it is hand-rolled over bun:sqlite
// rather than pulling the SDK — fewer deps, and it is verified by piping JSON-RPC at it.
import {
  openDb,
  defaultDbPath,
  listSessions,
  getSession,
  searchSessions,
} from "./db";

const PROTOCOL_VERSION = "2024-11-05";
const SERVER_INFO = { name: "plaude-local", version: "0.1.0" };

const db = openDb(defaultDbPath());

const TOOLS = [
  {
    name: "list_sessions",
    description:
      "List recent Plaude recording sessions, most recent first. Returns id, title, timestamp, status, a transcript snippet, and the speaker labels present.",
    inputSchema: {
      type: "object",
      properties: {
        limit: { type: "integer", minimum: 1, maximum: 100, description: "Max sessions (default 20)" },
        offset: { type: "integer", minimum: 0, description: "Skip this many (paging, default 0)" },
      },
    },
  },
  {
    name: "get_session",
    description:
      "Get one session's full transcript and speaker-attributed segments by id. Use to summarize a session or quote exactly who said what.",
    inputSchema: {
      type: "object",
      properties: { id: { type: "integer", description: "Session id from list_sessions/search_sessions" } },
      required: ["id"],
    },
  },
  {
    name: "search_sessions",
    description:
      "Search across every session's transcript and speaker segments (case-insensitive). Use to find where something was discussed; returns matching sessions with a snippet around the hit.",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string", description: "Text to look for" },
        limit: { type: "integer", minimum: 1, maximum: 100, description: "Max results (default 20)" },
      },
      required: ["query"],
    },
  },
];

function runTool(name: string, args: Record<string, unknown>): string {
  switch (name) {
    case "list_sessions":
      return JSON.stringify(
        listSessions(db, num(args.limit, 20), num(args.offset, 0)),
        null,
        2,
      );
    case "get_session": {
      const s = getSession(db, num(args.id, 0));
      return s ? JSON.stringify(s, null, 2) : `No session with id ${args.id}`;
    }
    case "search_sessions":
      return JSON.stringify(
        searchSessions(db, String(args.query ?? ""), num(args.limit, 20)),
        null,
        2,
      );
    default:
      throw new Error(`Unknown tool: ${name}`);
  }
}

function num(v: unknown, fallback: number): number {
  return typeof v === "number" && Number.isFinite(v) ? v : fallback;
}

function send(msg: unknown): void {
  process.stdout.write(JSON.stringify(msg) + "\n");
}

interface JsonRpc {
  id?: number | string | null;
  method?: string;
  params?: Record<string, unknown>;
}

function handle(req: JsonRpc): void {
  const { id, method, params } = req;
  const isRequest = id !== undefined && id !== null;
  try {
    switch (method) {
      case "initialize":
        send({
          jsonrpc: "2.0",
          id,
          result: {
            protocolVersion: PROTOCOL_VERSION,
            capabilities: { tools: {} },
            serverInfo: SERVER_INFO,
          },
        });
        break;
      case "notifications/initialized":
      case "initialized":
        break; // notification — no response
      case "ping":
        if (isRequest) send({ jsonrpc: "2.0", id, result: {} });
        break;
      case "tools/list":
        send({ jsonrpc: "2.0", id, result: { tools: TOOLS } });
        break;
      case "tools/call": {
        const text = runTool(
          String(params?.name ?? ""),
          (params?.arguments as Record<string, unknown>) ?? {},
        );
        send({ jsonrpc: "2.0", id, result: { content: [{ type: "text", text }] } });
        break;
      }
      default:
        if (isRequest)
          send({ jsonrpc: "2.0", id, error: { code: -32601, message: `Method not found: ${method}` } });
    }
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    if (!isRequest) return;
    // A tool failure is a normal result with isError, not a protocol error (MCP convention).
    if (method === "tools/call") {
      send({ jsonrpc: "2.0", id, result: { content: [{ type: "text", text: `Error: ${message}` }], isError: true } });
    } else {
      send({ jsonrpc: "2.0", id, error: { code: -32603, message } });
    }
  }
}

// Read newline-delimited JSON-RPC messages from stdin until the client closes it.
let buf = "";
const decoder = new TextDecoder();
for await (const chunk of Bun.stdin.stream()) {
  buf += decoder.decode(chunk, { stream: true });
  let nl: number;
  while ((nl = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, nl).trim();
    buf = buf.slice(nl + 1);
    if (!line) continue;
    let req: JsonRpc;
    try {
      req = JSON.parse(line);
    } catch {
      continue; // ignore malformed lines
    }
    handle(req);
  }
}
