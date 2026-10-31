# mcp-semantic-memory

A stateless MCP server (stdio) that stores and searches long-term semantic
memories. All state lives in an external SQL database, so any number of
instances can run side by side behind a load balancer. Written in Rust, built as
a static musl binary with Nix.

The project follows
[mcp-server-web](https://github.com/haras-unicorn/mcp-server-web) for tooling,
repository layout and docs. When unsure how something should be set up, look
there first. The search quality ideas come from
[mcp-memory-service](https://github.com/doobidoo/mcp-memory-service), used as a
reference only and not ported. It is not a knowledge graph; that would be a
separate MCP.

The implementation plan is done and gone. `docs/src/design.md` records what was
built and why. It is temporary and is deleted or folded into the docs once the
human has reviewed the code.

The core service also holds consolidation (`consolidation.rs`, with the
clustering in `cluster.rs`) and `reembed.rs`. Ranking, consolidation and the
startup checks use no wall clock: they measure time between memories, from the
median or the newest event, and timestamps come from the database. The optional
`memory_consolidate` tool is registered only when `consolidation.has_tool` is
on. While a startup check fails, every tool refuses, except that tool when the
only problems are the age of the consolidation or interests without a vector.

## Architecture

The server holds no state and no models. It talks to three external things, and
each one sits behind a facade (a small Rust trait) so the core logic never sees
a concrete implementation:

- **Embedder**: turns text into a vector. Implemented by an HTTP client for the
  OpenAI embeddings API, served in practice by a llama.cpp `llama-server`
  running an embedding model.
- **Reranker**: reorders candidate memories for a query. Implemented by an HTTP
  client for the Jina rerank API, served in practice by a second `llama-server`
  running a reranker model.
- **Store**: persists memories and does nearest-neighbour search. Implemented
  once per database dialect, each in its own crate.

Supported databases, each tested for real:

| Dialect  | Vector search   | Deployed as (target)   |
| -------- | --------------- | ---------------------- |
| Postgres | `pgvector`      | Patroni behind HAProxy |
| MariaDB  | native `VECTOR` | plain MariaDB          |
| SQLite   | `sqlite-vec`    | local or single node   |

Plain MySQL is not supported: Community Edition has no vector search. Patroni,
Vitess, Multigres and rqlite are not tested, and the docs claim only what the
tests above prove.

The core is a service that composes the three facades: it embeds on store,
fetches candidates from the store, reranks them and returns the result. The MCP
layer is a thin adapter over that service. Only the Store implementations
contain SQL: each uses sqlx with plain SQL and its own migration files, and
there is no ORM.

## Structure

Follows the layout of mcp-server-web.

- `src/crates/`: Cargo workspace members.
  - `mcp-semantic-memory`: the library. Config, core service, the three facades,
    the HTTP implementations of the embedder and reranker, and the MCP server.
    It holds no SQL and no database driver choice, and its default features are
    empty.
  - `mcp-semantic-memory-sqlite`, `-postgres`, `-mariadb`: one `Store` each,
    with its own `migrations/`, its own `.sqlx/` and its own sqlx features.
  - `mcp-semantic-memory-testing`: unpublished. The shared `Store` suite, the
    health checks, and (feature `containers`) the helpers that start test
    containers.
  - `mcp-semantic-memory-cli`: the binary `mcp-semantic-memory`, with every
    database and system TLS on by default. Its `main.rs` only sets up logging
    (`log.rs`), parses the arguments (`args.rs`), loads the config and hands
    both to `run.rs`, which picks the store from the URL scheme.
  - Unit tests sit beside the code; integration tests are in each crate's
    `tests/`.
- The lints and the package metadata are in the root `Cargo.toml`, and the crate
  inherits them. `clippy.toml` lets `#[test]` functions unwrap and panic.
- `src/nix/`: flake modules for `haraspkgs` `makeFlake`. Packages (the static
  binary, the docs, the test container images) and the dev shell live here.
- `docs/`: mdBook documentation. This is also where the list of tested databases
  is published.
- `flake.nix`: only wires `haraspkgs` and `./src`.

## Development

- Enter the dev shell with `nix develop` (or direnv). It provides the `dev`
  subcommands for test, lint, format and docs. `nix flake check` runs what CI
  runs.
- Cargo works offline against Nix-vendored crates, so changing dependencies
  needs network access to update `Cargo.lock`.
- The static-linkage check is a flake check in `src/nix/dev/static.nix` and not
  a dev shell check. The dev shell then does not depend on the package build,
  which needs `.sqlx` that `dev prepare` writes from inside the dev shell.
- Generated files (`.gitignore`, `LICENSE.md`, `.github/workflows/*`,
  `.cargo/config.toml` and similar) are produced by the dev shell. Edit the dev
  shell module (`src/nix/dev/shell/default.nix`, with the `dev prepare` script
  `prepare.nu` and the spelling `words.json` beside it), not the files.
- One test suite (in the testing crate) is written against the `Store` trait.
  SQLite runs it in memory. Postgres and MariaDB run it on real containers
  started by `testcontainers`. The `test` check runs all of them, and they fail
  if there is no container runtime. The check runs a `makePodmanScript` script
  (taken from `inputs.haraspkgs.lib.<system>.overlays.other`) that is given the
  images. In a Nix build it starts a podman daemon in the sandbox and loads
  them, which needs `uid-range` on the check derivation, so the dev shell module
  adds that feature to `checks.<system>.default-test` in a `perSystem` (the
  check module has no option for it). Under `dev check` the tests use the daemon
  of whoever runs them. So CI runs `nix flake check` alone. The HTTP facades are
  tested against `wiremock`.
- The test container images are built by Nix (`dockerTools`, in
  `src/nix/packages/test-images.nix`) and never pulled. The dev shell gives the
  tests their archive path and name in the
  `MCP_SEMANTIC_MEMORY_TEST_{POSTGRES,MARIADB}_{ARCHIVE,IMAGE}` variables, and
  the testing crate loads the archive into the runtime when the image is
  missing. Their server versions follow nixpkgs.
- The library returns typed errors made with `thiserror`. `anyhow` is for the
  binary parts: log, config, args and `main`.
- TLS is the system's (`native-tls`), never embedded. The library and the
  Postgres and MariaDB crates leave the choice to their users through a
  `native-tls` feature, off by default, which the binary turns on.
- The binary has `migrate`, `run` (migrates, then serves over stdio),
  `consolidate` (upkeep, run from a timer, a dry run unless told to apply) and
  `reembed` (after changing the embedding model; builds `memory_vectors_next`
  beside the live table, then swaps, with the swap done per dialect)
  subcommands. Stop `run` instances while re-embedding.
- Configuration uses the `config` crate: an optional file from the global
  `--config <path>` argument (TOML, YAML or JSON), which must exist, or else
  `mcp-semantic-memory.toml` in the current directory, if it is there. Then
  environment variables with the `MCP_SEMANTIC_MEMORY__` prefix, which win. `__`
  also goes into a section, so `search.limit` is
  `MCP_SEMANTIC_MEMORY__SEARCH__LIMIT`. The database URL is `database.url` and
  its scheme chooses the dialect. Durations are strings like `30d`, parsed by
  `humantime-serde`.
- Every tunable number (timeouts, pool sizes, limits, weights) comes from the
  configuration, never a literal or constant in the code. A constant is only for
  a fixed limit of a dependency, with a comment saying so.
- Logs are JSON on stderr from `log.rs`, with the event's fields at the top
  level next to a human-readable `message`.
- SQL is written inline in the function that runs it, so the query and what is
  done with its rows read together. Rows are read into small structs, not by
  column name.
- Queries use the sqlx `query!` family, which checks them against the schema
  while compiling. A crate can only check against one dialect, which is why each
  store is its own crate. The build does not connect to a database: it reads the
  crate's `.sqlx/`, which is committed and written by `dev prepare`. That
  command makes a SQLite file and starts Postgres and MariaDB in containers from
  the same Nix-built test images the tests use (through a `makePodmanScript`
  script, so it needs a podman service), makes databases out of each crate's
  migrations and runs `cargo check` on each crate. Run it after changing a query
  or a migration. Queries on the vector tables cannot be checked, since those
  databases cannot load the vector extension, so they use the plain `query`
  functions.
- Commits use Conventional Commits.
