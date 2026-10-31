# Introduction

mcp-semantic-memory is an [MCP](https://modelcontextprotocol.io) server that
gives an agent a long-term memory it can search by meaning.

The server itself is thin and keeps no state. Three things sit outside it:

- a database that holds the memories (SQLite, Postgres or MariaDB),
- an OpenAI-compatible server that turns text into vectors, like llama.cpp,
- optionally a Jina-style server that reranks search results, like a second
  llama.cpp.

Because of that, you can run as many instances as you like against one database,
for example behind a load balancer.

## How memory works

The agent does not edit memories directly. It tells the memory things, and each
thing it tells is an _event_. Every event is attached to a _memory_, a short
text that the agent rewrites to include what the event added. Events are never
changed or deleted, so the history of how a memory formed is always there.
Searching only looks at memories, so reading stays flat and fast.

Memories that nothing new attaches to fade away over time. If something new
comes up that is close to a faded memory, it comes back.

The full reasoning is in the [design](design.md).

## Where to go next

- [Servers](servers.md) shows how to run the embedder and the reranker.
- [Configuration](configuration.md) and [Databases](databases.md) cover the
  settings and the three databases.
- [Tools](tools.md) lists what the agent can call.
- [Maintenance](maintenance.md) covers consolidation and changing the embedding
  model.

## Status

Every database is tested on a real instance, and the docs list only what those
tests prove. The defaults are not tuned on real data yet, and consolidation has
not been timed on a large store. Treat it as young software.
