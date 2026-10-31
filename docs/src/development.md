# Where things are

The repository follows
[mcp-server-web](https://github.com/haras-unicorn/mcp-server-web).

- `src/crates/mcp-semantic-memory/`: the library. It holds the configuration,
  the core types, the three facades (embedder, reranker, store), the HTTP
  embedder and reranker, and the MCP server. `consolidation.rs` and `cluster.rs`
  do the upkeep, `reembed.rs` moves the store to a new embedding model.
  `fakes.rs` has in-memory versions of all three facades, compiled only for
  tests. It has no database code.
- `src/crates/mcp-semantic-memory-sqlite/`, `-postgres` and `-mariadb`: one
  store each, with its migrations. They are separate crates because sqlx checks
  the queries of a crate against one database dialect only.
- `src/crates/mcp-semantic-memory-testing/`: the store checks every store has to
  pass, and the helpers that start test containers. Not published.
- `src/crates/mcp-semantic-memory-cli/`: the binary, with all databases and
  system TLS turned on.
- `src/nix/`: the flake modules. `packages/` builds the static binary, these
  docs and the test container images, and `dev/shell/` defines the dev shell,
  its checks, formatters and the GitHub workflows.
- `docs/`: this book.
- `docs/src/design.md`: what was built and why. Temporary.

## Working on it

Enter the dev shell with `nix develop`, then use the `dev` command:

- `dev check` runs every check: tests, clippy, spelling, markdown and format.
- `dev format` formats the checkout.
- `dev docs` builds this book.
- `dev prepare` writes the `.sqlx/` folders.

`nix flake check` runs what CI runs. See [Contributing](contributing.md) and
[Releasing](releasing.md).

Generated files such as `.gitignore`, `LICENSE.md` and the workflows come from
the dev shell. Change `src/nix/dev/shell/default.nix` and not the files.

SQLite runs the store suite in memory, with fake facades. Postgres and MariaDB
run the same suite in real containers, as part of the `test` check. `dev check`
needs Podman or Docker for them and fails without it. Under `nix flake check`
the `test` check runs in a Nix build with the `uid-range` feature, starts a
podman daemon inside the sandbox and loads the images into it. The images are
built by Nix and handed to the tests through environment variables, so nothing
is pulled from a registry.

The queries are checked while compiling against the committed `.sqlx/` folder of
each store crate. After changing a query or a migration, run `dev prepare`: it
makes a throwaway SQLite file, starts Postgres and MariaDB in containers from
the same images as the tests, and writes the folders again. Like the tests, it
needs a podman service.
