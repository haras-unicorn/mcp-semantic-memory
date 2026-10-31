# mcp-semantic-memory

A stateless [MCP](https://modelcontextprotocol.io) server for long-term semantic
memory. Memories live in SQLite, Postgres or MariaDB, embeddings and reranking
come from llama.cpp servers, and the server itself keeps no state, so any number
of instances can share one database.

Documentation is an mdBook in [`docs/`](docs/src/introduction.md). Build it with
`dev docs`. The design is in [`docs/src/design.md`](docs/src/design.md).

## License

[MIT](LICENSE.md)
