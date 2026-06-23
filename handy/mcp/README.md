# Plaude Local — MCP server

A tiny, **dependency-free** [Model Context Protocol](https://modelcontextprotocol.io) server that
lets Claude connect to your recordings and **summarize sessions or find what was said** — entirely
on your Mac. It reads the app's SQLite database **read-only**, so it can never alter a recording,
and it speaks only over stdio (no network listener). Nothing leaves your machine.

## Tools

| Tool | What it does |
| --- | --- |
| `list_sessions` | Recent sessions (id, title, time, status, snippet, speaker labels). |
| `get_session` | One session's full transcript + speaker-attributed segments, by id. |
| `search_sessions` | Case-insensitive search across every transcript and segment; returns matches with a snippet around the hit. |

## Use it

**Claude Code** — already wired: the repo-root [`.mcp.json`](../../.mcp.json) registers it, so it
appears as the `plaude-local` server (approve it when prompted).

**Claude Desktop** — add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "plaude-local": {
      "command": "bun",
      "args": ["run", "/absolute/path/to/handy/mcp/server.ts"]
    }
  }
}
```

Then ask Claude things like *"summarize my last meeting"* or *"find where we talked about the budget."*

## Config & test

- DB path defaults to `~/Library/Application Support/com.pais.handy/history.db`. Override with the
  `PLAUDE_DB` env var (used by the tests).
- `bun test` runs the query-layer tests against an in-memory DB.
- Drive the protocol by hand: pipe newline-delimited JSON-RPC into `bun run server.ts` (see the
  recipe in [`docs/HANDOFF.md`](../../docs/HANDOFF.md) §7).

> ponytail: the MCP stdio handshake is small and stable, so it is hand-rolled over `bun:sqlite`
> rather than pulling the SDK. If the protocol grows, the official `@modelcontextprotocol/sdk` is
> the upgrade path.
