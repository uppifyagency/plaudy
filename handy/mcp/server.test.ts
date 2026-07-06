// Protocol tests: spawn the real `bun server.ts` with PLAUDE_DB pointed at a temp fixture
// DB and pipe newline-delimited JSON-RPC at it, exactly like an MCP client would.
import { expect, test } from "bun:test";
import { mkdtempSync } from "fs";
import { tmpdir } from "os";
import { join } from "path";
import { createDb, seedSessions } from "./fixture";

const SERVER = join(import.meta.dir, "server.ts");

function fixtureDbPath(): string {
  const path = join(
    mkdtempSync(join(tmpdir(), "plaude-mcp-test-")),
    "history.db",
  );
  const db = createDb(path);
  seedSessions(db);
  db.close();
  return path;
}

function spawnServer(dbPath: string) {
  const proc = Bun.spawn(["bun", SERVER], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "ignore",
    env: { ...process.env, PLAUDE_DB: dbPath },
  });
  const reader = proc.stdout.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  async function readMsg(): Promise<any> {
    for (;;) {
      const nl = buf.indexOf("\n");
      if (nl >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (line) return JSON.parse(line);
        continue;
      }
      const { value, done } = await reader.read();
      if (done) throw new Error("server closed stdout before responding");
      buf += decoder.decode(value, { stream: true });
    }
  }
  let nextId = 0;
  async function call(method: string, params?: unknown): Promise<any> {
    const id = ++nextId;
    proc.stdin.write(
      JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n",
    );
    proc.stdin.flush();
    const msg = await readMsg();
    expect(msg.id).toBe(id);
    return msg;
  }
  // tools/call sugar: returns the MCP tool result ({ content, isError? }).
  async function callTool(
    name: string,
    args: Record<string, unknown> = {},
  ): Promise<any> {
    return (await call("tools/call", { name, arguments: args })).result;
  }
  function rawLine(line: string): void {
    proc.stdin.write(line + "\n");
    proc.stdin.flush();
  }
  async function close(): Promise<void> {
    proc.stdin.end();
    await proc.exited;
  }
  async function initialize(): Promise<any> {
    const msg = await call("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "test", version: "0.0.0" },
    });
    rawLine(
      JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }),
    );
    return msg;
  }
  return { call, callTool, rawLine, close, initialize };
}

test("handshake, tools/list, tool calls, clamping, errors, malformed-line resilience", async () => {
  const srv = spawnServer(fixtureDbPath());
  try {
    const init = await srv.initialize();
    expect(init.result.protocolVersion).toBe("2024-11-05");
    expect(init.result.serverInfo.name).toBe("plaude-local");

    const tools = await srv.call("tools/list");
    expect(tools.result.tools.map((t: any) => t.name)).toEqual([
      "list_sessions",
      "get_session",
      "search_sessions",
    ]);

    // list_sessions with defaults returns both fixture sessions, newest first.
    let r = await srv.callTool("list_sessions");
    expect(JSON.parse(r.content[0].text).map((s: any) => s.id)).toEqual([2, 1]);

    // limit clamping: -1 (SQLite "unlimited"!), 0, and 1.5 all become 1.
    for (const limit of [-1, 0, 1.5]) {
      r = await srv.callTool("list_sessions", { limit });
      const rows = JSON.parse(r.content[0].text);
      expect(rows).toHaveLength(1);
      expect(rows[0].id).toBe(2);
    }
    // Huge limit is clamped (to 100) but still returns everything available.
    r = await srv.callTool("list_sessions", { limit: 999999 });
    expect(JSON.parse(r.content[0].text)).toHaveLength(2);

    // Offset paging works; negative offset clamps to 0.
    r = await srv.callTool("list_sessions", { limit: 1, offset: 1 });
    expect(JSON.parse(r.content[0].text).map((s: any) => s.id)).toEqual([1]);
    r = await srv.callTool("list_sessions", { offset: -5 });
    expect(JSON.parse(r.content[0].text)).toHaveLength(2);

    // Unknown tool → isError result, not a dead server.
    r = await srv.callTool("no_such_tool");
    expect(r.isError).toBe(true);
    expect(r.content[0].text).toContain("Unknown tool");

    // Empty / whitespace-only search query → friendly isError, not a LIKE '%%' full dump.
    r = await srv.callTool("search_sessions", { query: "   " });
    expect(r.isError).toBe(true);
    expect(r.content[0].text.toLowerCase()).toContain("empty");

    // A malformed JSON line is tolerated; the server keeps answering.
    srv.rawLine("{this is not json");
    r = await srv.callTool("search_sessions", { query: "budget" });
    expect(r.isError).toBeUndefined();
    expect(JSON.parse(r.content[0].text).map((h: any) => h.id)).toEqual([1]);
  } finally {
    await srv.close();
  }
});

test("missing history.db: initialize still succeeds, tool calls return a friendly error", async () => {
  const missing = join(
    mkdtempSync(join(tmpdir(), "plaude-mcp-nodb-")),
    "history.db",
  );
  const srv = spawnServer(missing);
  try {
    const init = await srv.initialize();
    expect(init.result.serverInfo.name).toBe("plaude-local");

    const r = await srv.callTool("list_sessions");
    expect(r.isError).toBe(true);
    expect(r.content[0].text).toContain("No recordings yet");
  } finally {
    await srv.close();
  }
});
