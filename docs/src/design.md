# Design

Decision record for the first full version. Short on purpose. It says what was
built and why.

## Goals

- Stateless: any number of stdio instances can run against one database. All
  state is in the database. Models live in separate servers.
- Flat semantic memory for reading: fast, easy for a small model, no graph.
- A permanent history of what was told to the memory, so the flat memory can be
  checked and rebuilt.
- Contradictions and duplicates are handled when a memory is written, by the
  model that is writing it, not guessed by the server.
- Reads never write.

## Data model

Three tables of record.

- **Event**: append-only, never changed. Fields: id, content, metadata (free
  form JSON; the model puts the session or occasion here), created at, the
  memory it was attached to, and the memory text this event produced. This is
  the history: what the model told the memory and what the memory became. Events
  are always attached to a memory; there are no pending events.
- **Memory**: the flat, searchable layer. Fields: id (primary key), content
  (always equal to the text of its latest event), content hash (unique, a safety
  net against exact duplicates), event count, last event at, interest score,
  forgotten at (empty while the memory is active), version (counts writes, used
  for the version check), created at, updated at. Many events attach to one
  memory. The memory is a projection of its events and can be rebuilt by
  replaying them, with no model.
- **Interest**: one row per interest per consolidation run. Fields: id, run at
  (shared by all rows of a run), top terms. Rows are never changed, so the table
  shows how interests moved over time. The current interests are the rows of the
  latest run.

Nothing is ever deleted. Forgetting sets `forgotten at`; search and recall skip
forgotten memories. This keeps every event linked to a memory, so the log never
has to be edited.

Vectors (memory embeddings, interest centroids) live in their own tables, keyed
by the memory or interest id. Each vector row carries the embedding fingerprint:
the embedding model label (from config), the dimension and the distance metric.
These tables are created from code with the configured dimension, because a
vector column needs its dimension up front. Everything else comes from migration
files, one set per dialect.

There is no knowledge graph. Search only reads memories. Events are read only
for audit and for rebuilding.

There are no tags and no memory type. The model writes prose; search finds it by
meaning and by words. The model's whole job is to link each event to the most
relevant memory and rewrite that memory's text. If consolidation ever needs
behaviour per kind of memory, add an enum then.

## Tools

Names follow the reference so existing prompts keep working.

- `memory_store`: the write path, described below. It is also the only way to
  change a memory, since every change is an event.
- `memory_search`: hybrid search over memories.
- `memory_recall`: fetch one memory by id. With `with_events` it also returns
  the events attached to it, newest first and capped (the newest matter the
  most), so the model can see how the memory formed over time.

There is no `memory_update` and no `memory_forget`. To correct or retire a
memory, the model stores an event that says so and rewrites the memory text.
Forgetting is only done by `consolidate`.

## Write path

The model tells the memory something new (an event). The server checks whether
it belongs to an existing memory and makes the model decide. This is the same
idea as the spec's multi round-trip requests (ask, then retry with the answer),
done with a plain tool call, because that spec feature can only ask the human
(elicitation) or use sampling, which is deprecated.

1. The model calls `memory_store` with the event content and metadata.
2. The server embeds the content and looks for memories closer than a
   configurable distance threshold. Forgotten memories are included.
3. If there are none, the server writes the event and a new memory in one short
   transaction and returns the new memory's id.
4. If there are some, the server writes nothing and returns them with a message:
   decide whether this is new, attaches to one of these (and write the updated
   memory text), or only repeats one of these.
5. The model calls `memory_store` again with the same event plus its decision.
   The server checks that the memories named still exist and have not changed
   since step 2 (a version check). If they changed, it answers as in step 4. If
   not, it writes in one short transaction:
   - new: the event, with the memory text it produced, and a new memory;
   - attach: the event, with the new memory text, and the memory's new text,
     embedding and hash;
   - repeat: only the event, attached to that memory with its text unchanged, so
     the event count goes up.

Attaching to a forgotten memory clears `forgotten at`. That is how an old memory
comes back.

Nothing is kept between the two calls, so any instance can answer either one. If
the model never makes the second call, the event is simply not recorded. That is
accepted: events are always attached, and an event the model drops did not
matter enough. The unique content hash only catches the rare race where two
instances write the same text at once.

## Search

Over memories only. The store returns two ranked lists, one by vector and one by
keyword. The service fuses them by weighted reciprocal rank: each list gives a
memory points by its position in it, and the vector list counts more. Then
frecency and the interest score are applied. Optional rerank through the
reranker facade on the top candidates. All weights, the candidate count and the
threshold are config. Keyword search is per dialect: Postgres full-text, MariaDB
FULLTEXT, SQLite FTS5.

- **Frecency** comes from writes only: the event count (how often the memory was
  reinforced) and the time of the last event. Searching does not change
  anything, so reads can go to replicas and hot memories do not fight over row
  locks.
- **Time is relative.** Recency, in frecency and in the consolidation weight, is
  not measured from the wall clock. It is measured from a reference, the median
  `last event at` of the memories being compared: the search candidates when
  searching, the sample when consolidating. A memory newer than the reference
  counts as fresh, and an older one decays by the half-life from there. So a
  quiet spell ages nothing, one new event after it does not make everything old
  at once (the median stays where most memories are), and instances with
  different clocks rank the same. The median, not the average, because a few
  very old or very new memories barely move it. For a steady user the median
  sits weeks back, so the newest half all look equally fresh and only the older
  half is told apart by age. That is accepted.
- **Interest score**: a memory's best cosine similarity to any centroid of the
  latest interest run. It is computed when the memory is written and stored on
  the memory; `consolidate` recomputes it for all memories after it finds new
  interests. With no interests yet, the score is neutral and ranking does not
  use it.

## Consolidation

A separate `consolidate` subcommand, run from a timer (a NixOS timer, for
example), never at startup. It has no model to talk to and is meant to stay that
way: it is deliberately automatic. It defaults to a dry run that reports what it
would do. It writes no events; forgetting is maintenance of the flat layer, not
something the model was told.

For a deployment with no timer, where the whole thing is the binary and one
SQLite file, `consolidation.has_tool` adds a fourth tool, `memory_consolidate`.
It takes no arguments, runs the same run and applies it (the dry run is for the
subcommand only), and answers with the same report. It is not for routine use:
its description tells the model to call it only after another tool failed
because the last consolidation is too old. The setting is on by default, so the
one-file deployment works with no configuration. A deployment with a timer sets
it to off, and then the tool is not listed at all, so the model never sees it. A
model that can store memories well enough to be worth having can run this too,
and a run is not destructive: with the same data, a second run finds the same
interests and forgets nothing more. That holds in practice and is tested, but it
is not proven: it rests on the sample not changing.

The tool changes the startup check on age. With the tool on, the age problem
names `memory_consolidate`, and that tool still works while the others fail.
With the tool off, it names the `consolidate` command as before. Memories
without a current vector still block every tool, `memory_consolidate` included,
since clustering mixed vectors means nothing; only `reembed` fixes those.
Interests without a current vector are rebuilt by consolidation, so the tool is
allowed for them. Two instances may be told to consolidate at once, so the run
takes a lock (per dialect, like the migrations), checks the age again under it,
and does nothing if the memory is no longer stale.

Steps of a run:

1. Find interests. Take the top N active memories, where the store orders by a
   proxy of the weight (event count, then the time of the last event), since the
   decay needs the median and so cannot be done in SQL. Then
   `weight = ln(1 + event count) * recency decay` (half-life in config, from the
   median of the sample as under Search). Run weighted k-means on their
   embeddings, in plain Rust. The weight shapes both the starting points and
   each memory's pull on its cluster's centre. `k` is the natural log of the
   memory count (base configurable), clamped to a min and max. Nothing is
   random: the first starting centre is the heaviest memory and each next one is
   the memory with the most weight times distance from the centres so far, so
   there is no seed to store (a first version used k-means++ with a stored seed
   and was dropped for this). For each centre, extract its top terms: split the
   text of the cluster's memories into lowercase words, score each word by its
   weighted frequency in the cluster times how rare it is across all active
   memories, read in pages (so filler words score near zero in any language,
   with no stopword list), and keep the best few. Write one interest row per
   centre.
2. Recompute every memory's interest score against the new centroids, in
   batches.
3. Forget: set `forgotten at` on memories that are old enough (a grace period,
   so new topics are not forgotten before they can become interests; measured
   from the newest event, so an idle server ages nothing) and low on both
   frecency and interest score. A memory that is never reinforced by a new event
   fades. That is intended: if nothing new and meaningful has attached to it,
   something closer to a current interest has taken its place. There is no
   switch to disable forgetting and none to protect single memories. Frecency
   here uses `search.half_life`, since consolidation has no half-life of its
   own.

The dry run prints the interests it would create, each labelled with its top
terms and the text of the memory closest to its centre, and the memories it
would forget.

## Commands and config

Stdio only. Subcommands (clap derive): `migrate`, `run` (migrates, then serves),
`consolidate`, `reembed`. Config is an optional file (`--config <path>`, TOML,
YAML or JSON) and then environment variables, which win. There is one database
URL, `database.url` (`MCP_SEMANTIC_MEMORY__DATABASE__URL`), and the dialect
comes from its scheme. Embedding and rerank server URLs are the other two. Every
tunable number is config; none is a constant in the code.

`reembed` builds a new memory vector table beside the live one, fills it page by
page by embedding each memory's text with the current embedder, and then swaps
the tables. It never loads everything at once, it can be stopped and resumed (it
continues with the memories that have no row of the current fingerprint in the
new table), and the live table is untouched until the swap. A leftover new table
is kept only if it has such rows, and otherwise it is recreated, which also
handles a new dimension. Forgotten memories are embedded too, so a revival needs
no embedding. Before the swap it embeds any memory that is still missing.

The swap differs per dialect:

- Postgres: one transaction that takes an advisory lock and a share lock on
  `memories`, drops the old table, and renames the new table and its indexes.
- MariaDB cannot do that in a transaction, so it takes a named lock, drops the
  interest vectors, renames both tables in one atomic statement and drops the
  old one afterwards. A memory written between the check for missing vectors and
  the rename could be left without a vector; stopping the instances avoids it.
- SQLite: renaming a `vec0` virtual table breaks sqlite-vec queries, so one
  transaction drops the live table, recreates it and copies the rows over.

The interest vectors are dropped and `interest_vectors` is recreated with the
new dimension, since the old ones are in the old space; the interest history
rows stay. `consolidate` must run next to rebuild the interests. Stop `run`
instances while re-embedding: the changed config makes them refuse anyway, and
there is no per-memory version guard on the vectors it writes.

Startup checks, done by `run`:

- Every active memory has a vector, and every interest of the latest run has a
  vector, all with the current embedding fingerprint. Otherwise `run` refuses to
  serve until `reembed` (memories) or `consolidate` (interests) has been run.
- The latest consolidation is not too far behind the activity: the newest event
  is no more than a maximum age (config, default two weeks) later than the
  latest interest row. If there are no interest rows yet, the oldest active
  memory stands in for that row. With no active memories there is nothing to
  consolidate and the check passes, and so it does when nothing has happened
  since the last run. Both times come from the database. A run on at least one
  memory always writes at least one interest row, so no separate run record is
  needed. This catches a forgotten timer, but not an idle server, which has
  nothing to consolidate.

When a check fails, `run` still starts and answers the handshake, and every tool
call returns the reason as its error (for example "run `reembed`"), so the model
and the human both see it. `migrate` and `reembed` are never blocked by these
checks. The `consolidate` command and tool are blocked only by memories that
need a reembed, since clustering mixed vectors means nothing. The fingerprint
comes from config, so swapping the model file behind an unchanged label is not
detected.

## Facades

Embedder, Reranker and Store, as in `AGENTS.md`. The core service knows only
those traits. The Embedder takes a kind, query or document, because many
embedding models expect different prefixes for each; the prefixes are config.
Each Store implementation uses sqlx with plain SQL, checked while compiling
where the database allows it; there is no ORM. It is three tables and a few
queries per dialect. The compile-time check reads one crate's `.sqlx` for one
dialect, so each Store is its own crate, and the library crate holds no SQL. The
stores are allowed to differ where their databases do (keyword matching, limits
on vectors, how a write race is caught); `databases.md` lists the differences.
The invariants of the data model hold on every store.

## Testing

- One test suite is written against the `Store` trait, in its own crate. SQLite
  runs it in memory with fake facades. Postgres and MariaDB run the same suite
  on real containers started by `testcontainers`. The images are built by Nix,
  so the run is the same everywhere. In the dev shell the `test` check requires
  them and fails without a container runtime. Under `nix flake check` the `test`
  check gets a podman daemon in the build sandbox for them. CI runs
  `nix flake check` alone. The suite also has the write races (two writers of
  the same text, two attaches to one memory) and the health numbers, which need
  the database's clock and are tested by editing times directly.
- Smoke tests for Patroni and Vitess are a follow-up. Their images are in
  haraspkgs now, but the tests are not written. rqlite needs a backend of its
  own, and Multigres waits until it exists. Until then the docs claim only
  Postgres, MariaDB and SQLite.
- A fake embedder maps chosen words to hand-picked vectors so scenarios (a
  repeat, a contradiction, a resurrection, a version conflict) can be scripted.
- Defaults for thresholds and weights should come from measurements, not from
  guessing. `quality.md` says how, with synthetic data only.

## Rationale and holes

The design seen as a whole: what each choice solves, and where it can still
break. Status is one of **decided**, **proposed** (needs approval, not yet
applied above) or **accepted** (a known risk we live with).

### Holes found, and what was done

1. **Memory text has no history.** Decided: each event stores the memory text it
   produced, so the memory is a projection of the log and can be rebuilt by
   replay.
2. **Duplicate memories cannot be retired.** With no forget tool, two memories
   about one thing both stay in search. Accepted: duplicates come from a race or
   from the model misjudging where an event belongs, both rare, and merging
   would make large memories. An unreinforced duplicate fades. If real use shows
   duplicates are common, a merge decision that sets `forgotten at` on the
   absorbed memories can be added later with no schema change.
3. **Static facts fade.** A fact told once and only read has one event and ages
   out. Accepted, on purpose: only new events reinforce a memory, like human
   memory. No capacity switch and no never-forget switch.
4. **An abandoned write loses the event.** Accepted: events are always attached
   to a memory, the schema is not loosened, and the model is trusted to finish
   what it started.
5. **Search score scales differ per dialect.** Decided: the store returns ranked
   lists and the service fuses them by rank, so dialect differences stay behind
   the trait.
6. **Changing the embedding model has no path.** Decided: `reembed`, then
   `consolidate`, and a startup check that refuses to run on mismatched rows.
7. **Memory text can grow without bound.** The model rewrites the text on every
   attach and may keep appending. Embedding servers have a context limit and may
   truncate silently, which makes long memories embed badly. Decided: config
   sets a maximum length for event and memory text; over it, the store answers
   with an error telling the model to condense.
8. **Vector columns need the dimension up front.** Postgres `vector(n)`, MariaDB
   `VECTOR(n)` and SQLite `vec0` all fix `n` when created, but static migration
   files cannot know it. Decided: no ORM, sqlx with plain SQL; vectors live in
   their own tables created from code with the configured dimension; everything
   else comes from migration files per dialect. `reembed` builds a new vector
   table beside the old one and swaps them.
9. **Embedding prefixes.** Decided: the Embedder takes a kind (query or
   document), and config holds an optional prefix for each, because some
   embedding models expect different text prefixes for queries and documents.

### Decided, and why

- **Event log plus flat memories**: contradictions and drift are fixed where the
  knowledge is, in the model writing the memory, while history is never lost and
  reads stay flat and cheap for small models. No graph, because a small model
  gets lost in one.
- **Two-call write path with a plain tool call**: the spec's multi round-trip
  requests can only ask the human or use deprecated sampling, so the model is
  asked through an ordinary tool result. Works in every client and keeps
  instances stateless.
- **Reads never write**: reads can go to replicas, hot rows do not contend, and
  single-writer databases (SQLite, rqlite) are not hit on every search.
- **No tags, no type**: the model writes prose; search covers meaning and words.
  The model's job stays small.
- **Interests by weighted clustering of the memories themselves**: they follow
  the user without config and without a model; the table keeps their history.
  The risk is that interests reinforce themselves; the grace period and the
  both-low rule limit it.
- **Stdio only**: the server is thin, the heavy work is in the three external
  servers, so many cheap instances are fine.
- **Vectors sent as text and cast in SQL**: no driver-specific vector types.
- **Nothing deleted**: forgotten is a timestamp, so every event keeps its memory
  and the log is never edited.

### Accepted risks

- **Model quality.** Consolidated text is summarization plus retrieval, so it is
  as good as the model writing it. The log makes mistakes recoverable, not
  impossible.
- **Near-duplicate race.** Two instances can create near-identical new memories
  at the same moment; the version check cannot see a memory that does not exist
  yet. The next nearby event shows both to the model.
- **Threshold and candidate flood.** A bad distance threshold gives too many or
  too few candidates. Cap the number returned, cap `with_events` and response
  size, and tune with the measurements in `quality.md`.
- **Keyword search differs per dialect.** Use the language-neutral Postgres
  `simple` configuration; MariaDB and SQLite have their own tokenizing and
  minimum word length. Documented, not hidden.
- **Memory poisoning and tenancy.** Anything the model stores comes back in
  later sessions, including injected text from a web page. Event metadata
  (source, occasion) helps audit. There is no auth and no isolation: one
  database and one server per agent.
- **Failures of the outside servers.** If the embedder is down, store and search
  fail with a clear error. If the reranker is down, search skips reranking and
  warns, as the reference does.
- **Failover and clocks.** Writes are not retried automatically, since a retry
  after an unknown outcome could write an event twice; the error goes to the
  model. Timestamps come from the database, not from instance clocks, and
  ranking and forgetting use no clock at all: they compare memories with each
  other (see Search).
- **Loops.** A model can keep calling `memory_store` without a decision. The
  server just repeats the candidates; it costs tokens, not data.
- **Interest bubble.** Ranking by interest favours what is already frequent. The
  weight is config and small by default.
- **Late bugs on other databases.** Postgres and MariaDB are tested for real,
  but only the versions the test images carry, and over TLS only with a
  certificate nobody signed. Other servers are untested, and smoke tests will
  surface their bugs. Per-dialect SQL stays small to keep that cheap.

## Open questions

- Defaults for the search weights, the distance threshold and the consolidation
  parameters (sample size, half-life, k bounds, grace period, forgetting
  thresholds, maximum text length, maximum consolidation age). They are config;
  tune with real data.
