# Databases

The server keeps its memories in SQLite, Postgres or MariaDB. The scheme of
`database.url` picks which one. All three hold the same data and follow the same
rules: events are never changed, a memory always equals its latest event, a
write that lost a race is refused and not half done. They do not behave
identically, because the databases cannot. This page lists the differences.

| Database | Vector search          | Needs                         |
| -------- | ---------------------- | ----------------------------- |
| SQLite   | `sqlite-vec`, built in | nothing                       |
| Postgres | `pgvector`, HNSW index | `pgvector` 0.8 or newer       |
| MariaDB  | native `VECTOR`        | MariaDB 11.7 or newer         |
| MySQL    | not supported          | Community has no vector index |

The `mcp-semantic-memory` crate holds only the core: the traits, the service,
the HTTP embedder and reranker, and the MCP server. The stores are separate
crates, `mcp-semantic-memory-sqlite`, `-postgres` and `-mariadb`, so a library
user who wants a real store depends on the core and on the store crates they
need. The `mcp-semantic-memory-cli` package builds the `mcp-semantic-memory`
binary with all of them.

Each of the three is tested against the same suite. SQLite runs in memory.
Postgres and MariaDB run on real servers in containers. Other servers that speak
these dialects, like Patroni, Vitess or Multigres, are not claimed to work: none
of them has been tested. See [Other databases](#other-databases).

## Keyword search

The keyword half of a search is done by each database's own full-text search, so
what counts as a word differs.

- **SQLite** uses FTS5 with the `unicode61` tokenizer. Accents are removed, so
  `café` matches `cafe`.
- **Postgres** uses the `simple` configuration: no stemming and no stopwords, so
  it works for any language. Accents are kept, so `café` does not match `cafe`.
- **MariaDB** uses InnoDB `FULLTEXT`. Words shorter than three letters and its
  built-in stopwords are ignored. A search made only of such words finds nothing
  by keyword, and the vector half still answers.

The server fuses the two halves by rank, not by score, so the different scales
do not matter.

## Vectors

- **Postgres:** the HNSW index of `pgvector` holds at most 2000 dimensions, so
  `embedding.dimension` cannot be larger. Nearest-neighbour searches use
  `hnsw.iterative_scan = strict_order`, so that forgotten memories do not use up
  the limit. That setting is why `pgvector` 0.8 is needed.
- **MariaDB:** the vector search over-fetches by the number of forgotten
  memories, so forgotten ones do not push live ones out of the limit.
- **SQLite:** nothing special, but there is a single writer.

## Writes and locks

- **Postgres** takes an advisory lock per content hash on writes, so two
  instances writing the same text at once cannot both succeed. A lock also
  guards creating the vector tables.
- **MariaDB** relies on the unique content hash, runs at `READ COMMITTED`, and
  turns the duplicate-key error into a refusal. A named lock guards creating the
  vector tables.
- **SQLite** has one writer at a time, which gives the same guarantee.

## Consolidation

A consolidation writes the new interests and the verdicts on the memories in one
transaction, under a lock that only one instance can hold. That is how two
instances told to consolidate at once consolidate once: the second finds the
first one's run when it gets the lock and does nothing.

- **Postgres** takes an advisory lock for the length of the transaction, and
  takes the time of the run from `clock_timestamp()`.
- **MariaDB** takes a named lock (`GET_LOCK`) on one connection and gives it
  back on every path, with a transaction inside.
- **SQLite** takes the write lock (`BEGIN IMMEDIATE`) before it reads anything.

The run time is at least a tiny step after the run before, so two runs never
share a time. Rows of the same run share one.

## Reembed

`reembed` fills a table of new vectors beside the live one and swaps them. The
swap differs, because the databases do.

- **Postgres** drops the old table and renames the new one inside one
  transaction. It holds a lock on the memories for the swap, so no memory can be
  written between the check that nothing is missing and the swap.
- **MariaDB** cannot change tables in a transaction, so it renames both in one
  atomic statement and drops the old one afterwards. The interest vectors are
  reset first, so a run that stops half way can be run again.
- **SQLite** cannot rename a `sqlite-vec` table: the queries on it fail
  afterwards. It drops the live table, makes it again and copies the new vectors
  in, all in one transaction. That takes longer than a rename, in proportion to
  the number of memories, and holds the write lock meanwhile.

The new table is called `memory_vectors_next`. If a run stops, it stays with
what was done, and the next run goes on from there. A table left by a run with
another model is dropped, since nothing in it can be used.

## Times

Postgres stores times as timestamps with a time zone, MariaDB as `TIMESTAMP(6)`
with the connection set to UTC, and SQLite as text. All times come from the
database, not from the instance.

## TLS

The binary uses the system's TLS, never an embedded one, and trusts the
certificates the machine trusts. Add a private CA to the system's store and not
to this program. Ask for it in the URL: `?sslmode=require` or `verify-full` on
Postgres, `?ssl-mode=required` or `verify_identity` on MariaDB. SQLite has no
network and no TLS.

The tests start Postgres and MariaDB with a certificate nobody signed. They show
that `require` gives an encrypted session, that the store works over it, and
that `verify-full` and `verify_identity` refuse the certificate. A certificate
that does verify, from a private CA, is not tested.

The library crates leave TLS off. A program that uses a store crate turns it on
with the `native-tls` feature.

## Other databases

Only plain Postgres, plain MariaDB and SQLite are tested. Patroni (behind
HAProxy), Vitess and Multigres speak the Postgres and MySQL protocols, but there
is no test setup for them yet and none is claimed to work. They are not expected
to work without a look: the MariaDB store needs `VECTOR` columns, `VEC_*`
functions and named locks, and the Postgres store needs `pgvector` and advisory
locks.

**rqlite** exposes an HTTP API and not the SQLite protocol, so the SQLite store
cannot talk to it. Supporting it would take a store of its own.

## Failover

When the database goes away, as when a Patroni leader changes, the calls that
need it fail with an error after `database.acquire_timeout`. The pool replaces
broken connections when it needs one, so calls work again once the database is
reachable. An instance does not retry by itself. It cannot know what is going on
with the failover, and the model or the person on the other side can, so the
error goes to them and they decide whether to try again. This has not been
tested against a real failover.

## Versions of the test servers

The images the tests use are built by Nix, so the server versions follow
nixpkgs: `pgvector` 0.8.x on Postgres and MariaDB 11.8. That is what is proven;
other versions inside the requirements above are expected to work.
